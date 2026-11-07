//! The client's microphone, offered to the remote desktop as a PipeWire
//! source named "Gliff microphone". `pw-cli` loads a pipe-tunnel source that
//! reads raw samples from a FIFO, and the decoded packets are written into
//! it. The source is the desktop's default input while it exists.

use std::path::{Path, PathBuf};

use anyhow::{ensure, Context, Result};
use gliff_audio::{CHANNELS, FRAME, SAMPLE_RATE};
use tokio::net::unix::pipe;
use tokio::process::{Child, Command};

use crate::audio::command;

pub const MAX_PACKET: u32 = 4000;
const DEFAULT_KEY: &str = "default.configured.audio.source";
/// One frame of f32 samples: under `PIPE_BUF`, so a write of it is atomic and
/// a full FIFO drops whole frames, never half of one.
const FRAME_BYTES: usize = FRAME * CHANNELS * 4;
/// Silence queued ahead of the first packet, for network jitter to drain.
const CUSHION_FRAMES: usize = 6;

#[derive(Default)]
pub struct Mic {
    source: Option<Source>,
}

impl Mic {
    pub fn start(&mut self, codec: gliff_proto::AudioCodec, rate: u32, channels: u32) {
        if let Err(e) = check_format(codec, rate, channels) {
            tracing::warn!(error = %e, "microphone refused");
            self.source = None;
            return;
        }
        if let Some(source) = &mut self.source {
            match gliff_audio::Decoder::new() {
                Ok(decoder) => source.decoder = decoder,
                Err(e) => tracing::warn!(error = %e, "cannot reset the microphone decoder"),
            }
            return;
        }
        match Source::start() {
            Ok(source) => {
                tracing::info!(node = %source.node, "microphone on");
                self.source = Some(source);
            }
            Err(e) => tracing::warn!(error = %format!("{e:#}"), "microphone unavailable"),
        }
    }

    pub fn stop(&mut self) {
        if self.source.take().is_some() {
            tracing::info!("microphone off");
        }
    }

    pub fn frame(&mut self, seq: u32, packet: &[u8]) {
        if let Some(source) = &mut self.source {
            source.frame(seq, packet);
        }
    }
}

fn check_format(codec: gliff_proto::AudioCodec, rate: u32, channels: u32) -> Result<()> {
    ensure!(
        codec == gliff_proto::AudioCodec::Opus,
        "unknown codec {codec:?}"
    );
    ensure!(
        rate == SAMPLE_RATE && channels as usize == CHANNELS,
        "unsupported format {rate} Hz, {channels} channels"
    );
    Ok(())
}

struct Source {
    node: String,
    path: PathBuf,
    fifo: pipe::Sender,
    _tunnel: Child,
    decoder: gliff_audio::Decoder,
    /// The configured default source before ours, as its raw JSON value.
    previous_default: Option<String>,
    bytes: Vec<u8>,
    dropping: bool,
}

impl Source {
    fn start() -> Result<Self> {
        let node = format!("gliff-mic-{}", std::process::id());
        let dir = match std::env::var_os("XDG_RUNTIME_DIR") {
            Some(dir) => PathBuf::from(dir),
            None => hypr_ipc::runtime_dir().context("no runtime directory")?,
        };
        let path = dir.join(format!("{node}.fifo"));
        let _ = std::fs::remove_file(&path);
        nix::unistd::mkfifo(
            &path,
            nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
        )
        .context("create the microphone FIFO")?;
        let result = Self::open(node, path.clone());
        if result.is_err() {
            let _ = std::fs::remove_file(&path);
        }
        result
    }

    fn open(node: String, path: PathBuf) -> Result<Self> {
        // Read-write, so opening does not wait for the tunnel to open its end.
        let fifo = pipe::OpenOptions::new()
            .read_write(true)
            .open_sender(&path)
            .context("open the microphone FIFO")?;
        let mut pw = command("pw-cli");
        pw.args([
            "-m",
            "load-module",
            "libpipewire-module-pipe-tunnel",
            &tunnel_args(&node, &path),
        ])
        .stdout(std::process::Stdio::null());
        let tunnel = Command::from(pw)
            .kill_on_drop(true)
            .spawn()
            .context("start pw-cli")?;
        let previous_default = read_default();
        set_default(&format!("{{\"name\":\"{node}\"}}"));
        let mut source = Self {
            node,
            path,
            fifo,
            _tunnel: tunnel,
            decoder: gliff_audio::Decoder::new()?,
            previous_default,
            bytes: Vec::with_capacity(FRAME_BYTES * 5),
            dropping: false,
        };
        source.write(&vec![0.0; FRAME * CHANNELS * CUSHION_FRAMES]);
        Ok(source)
    }

    fn frame(&mut self, seq: u32, packet: &[u8]) {
        let pcm = match self.decoder.decode(seq, packet) {
            Ok(pcm) => pcm.to_vec(),
            Err(e) => {
                tracing::debug!(error = %e, "microphone packet dropped");
                return;
            }
        };
        self.write(&pcm);
    }

    fn write(&mut self, pcm: &[f32]) {
        self.bytes.clear();
        self.bytes.extend(pcm.iter().flat_map(|s| s.to_le_bytes()));
        for chunk in self.bytes.chunks(FRAME_BYTES) {
            match self.fifo.try_write(chunk) {
                Ok(_) => self.dropping = false,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if !self.dropping {
                        tracing::debug!("microphone FIFO full; dropping");
                        self.dropping = true;
                    }
                    return;
                }
                Err(e) => {
                    tracing::warn!(error = %e, "microphone FIFO write failed");
                    return;
                }
            }
        }
    }
}

impl Drop for Source {
    fn drop(&mut self) {
        let ours = format!("{{\"name\":\"{}\"}}", self.node);
        if read_default().is_some_and(|current| same_node(&current, &ours)) {
            match &self.previous_default {
                Some(previous) => set_default(previous),
                None => {
                    let _ = command("pw-metadata")
                        .args(["-d", "0", DEFAULT_KEY])
                        .stdout(std::process::Stdio::null())
                        .status();
                }
            }
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

fn tunnel_args(node: &str, path: &Path) -> String {
    format!(
        "{{ tunnel.mode=source pipe.filename=\"{}\" audio.format=F32LE audio.rate={SAMPLE_RATE} \
         audio.channels={CHANNELS} audio.position=[FL FR] node.name={node} \
         node.description=\"Gliff microphone\" media.class=Audio/Source }}",
        path.display()
    )
}

fn read_default() -> Option<String> {
    let out = command("pw-metadata")
        .args(["0", DEFAULT_KEY])
        .stdout(std::process::Stdio::piped())
        .output()
        .ok()?;
    parse_default(&String::from_utf8_lossy(&out.stdout))
}

fn set_default(value: &str) {
    let _ = command("pw-metadata")
        .args(["0", DEFAULT_KEY, value, "Spa:String:JSON"])
        .stdout(std::process::Stdio::null())
        .status();
}

/// The value of the default source key in `pw-metadata` output.
fn parse_default(output: &str) -> Option<String> {
    let marker = format!("key:'{DEFAULT_KEY}' value:'");
    output.lines().find_map(|line| {
        let rest = &line[line.find(&marker)? + marker.len()..];
        Some(rest[..rest.find("' type:")?].to_string())
    })
}

fn same_node(a: &str, b: &str) -> bool {
    let name = |v: &str| {
        serde_json::from_str::<serde_json::Value>(v)
            .ok()
            .and_then(|v| v["name"].as_str().map(str::to_owned))
    };
    name(a).is_some() && name(a) == name(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    const OUTPUT: &str = "Found \"default\" metadata 40\n\
        update: id:0 key:'default.configured.audio.source' \
        value:'{\"name\":\"alsa_input.usb-mic\"}' type:'Spa:String:JSON'\n";

    #[test]
    fn the_configured_default_is_read_from_pw_metadata() {
        assert_eq!(
            parse_default(OUTPUT).as_deref(),
            Some(r#"{"name":"alsa_input.usb-mic"}"#)
        );
        assert_eq!(parse_default("Found \"default\" metadata 40\n"), None);
    }

    #[test]
    fn defaults_compare_by_node_name() {
        assert!(same_node(
            r#"{"name":"gliff-mic-1"}"#,
            r#"{ "name": "gliff-mic-1" }"#
        ));
        assert!(!same_node(
            r#"{"name":"gliff-mic-1"}"#,
            r#"{"name":"gliff-mic-2"}"#
        ));
        assert!(!same_node("not json", "not json"));
    }

    #[test]
    fn only_the_stream_format_is_accepted() {
        assert!(check_format(gliff_proto::AudioCodec::Opus, 48_000, 2).is_ok());
        assert!(check_format(gliff_proto::AudioCodec::Opus, 44_100, 2).is_err());
        assert!(check_format(gliff_proto::AudioCodec::Opus, 48_000, 1).is_err());
    }

    #[test]
    fn the_tunnel_reads_the_fifo_as_a_named_source() {
        let args = tunnel_args("gliff-mic-7", Path::new("/run/user/1000/gliff-mic-7.fifo"));
        assert!(args.contains(r#"pipe.filename="/run/user/1000/gliff-mic-7.fifo""#));
        assert!(args.contains("tunnel.mode=source"));
        assert!(args.contains("node.name=gliff-mic-7"));
        assert!(args.contains("audio.format=F32LE audio.rate=48000 audio.channels=2"));
    }
}
