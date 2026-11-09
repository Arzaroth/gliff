//! This machine's microphone, recorded by `pw-record` from the default input
//! and coded as Opus for the server's virtual source.

use anyhow::{ensure, Context, Result};
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

/// Coded packets waiting to be sent; past half a second the newest are
/// dropped and the server conceals the gap.
const QUEUE: usize = 50;
const SERVER_NODE_PREFIX: &str = "gliff-mic-";

pub struct Capture {
    _child: Child,
    pub packets: mpsc::Receiver<(u32, Vec<u8>)>,
}

impl Capture {
    /// Record `source`, or the default input when it is `None`. The capture
    /// stays on that input: following a later default change could pick up
    /// the Gliff microphone of a machine showing this one, and send its voice
    /// back.
    pub fn start(source: Option<&str>) -> Result<Self> {
        let target = match source {
            Some(name) => Some(name.to_owned()),
            None => default_source(),
        };
        ensure!(
            !target.as_deref().is_some_and(is_gliff_source),
            "the microphone is another session's Gliff microphone"
        );
        let rate = gliff_audio::SAMPLE_RATE.to_string();
        let channels = gliff_audio::CHANNELS.to_string();
        let mut cmd = Command::new("pw-record");
        if let Some(target) = &target {
            cmd.args(["--target", target]);
        }
        let mut child = cmd
            .args([
                "--raw",
                "--rate",
                &rate,
                "--channels",
                &channels,
                "--format",
                "f32",
                "--latency",
                "10ms",
                "-P",
                "{ node.name=gliff-capture node.description=Gliff media.role=Communication }",
                "-",
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .context("start pw-record")?;
        let mut stdout = child.stdout.take().context("pw-record has no output")?;
        let mut encoder = gliff_audio::Encoder::new()?;
        let (tx, packets) = mpsc::channel(QUEUE);
        tokio::task::spawn_local(async move {
            let mut bytes = vec![0u8; gliff_audio::FRAME * gliff_audio::CHANNELS * 4];
            let mut have = 0;
            let mut pcm = Vec::with_capacity(bytes.len() / 4);
            let mut seq: u32 = 0;
            loop {
                match stdout.read(&mut bytes[have..]).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => have += n,
                }
                let whole = have / 4 * 4;
                pcm.clear();
                pcm.extend(
                    bytes[..whole]
                        .chunks_exact(4)
                        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])),
                );
                bytes.copy_within(whole..have, 0);
                have -= whole;
                let coded = match encoder.push(&pcm) {
                    Ok(p) => p,
                    Err(e) => {
                        tracing::warn!(error = %e, "microphone encode failed");
                        break;
                    }
                };
                for packet in coded {
                    let numbered = (seq, packet);
                    seq = seq.wrapping_add(1);
                    if let Err(mpsc::error::TrySendError::Closed(_)) = tx.try_send(numbered) {
                        return;
                    }
                }
            }
            tracing::info!("microphone capture ended");
        });
        Ok(Self {
            _child: child,
            packets,
        })
    }
}

fn default_source() -> Option<String> {
    let out = std::process::Command::new("pw-metadata")
        .args(["0", "default.audio.source"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    parse_default_name(&String::from_utf8_lossy(&out.stdout))
}

/// The node name in `pw-metadata`'s line for the default source.
fn parse_default_name(output: &str) -> Option<String> {
    output.lines().find_map(|line| {
        let value = &line[line.find("value:'")? + "value:'".len()..];
        let value = &value[..value.find("' type:")?];
        serde_json::from_str::<serde_json::Value>(value).ok()?["name"]
            .as_str()
            .map(str::to_owned)
    })
}

fn is_gliff_source(name: &str) -> bool {
    name.starts_with(SERVER_NODE_PREFIX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_source_name_is_read_from_pw_metadata() {
        let out = "Found \"default\" metadata 40\n\
            update: id:0 key:'default.audio.source' \
            value:'{\"name\":\"bluez_input.98:FD\"}' type:'Spa:String:JSON'\n";
        assert_eq!(
            parse_default_name(out).as_deref(),
            Some("bluez_input.98:FD")
        );
        assert_eq!(parse_default_name(""), None);
    }

    #[test]
    fn a_gliff_microphone_is_never_recorded() {
        assert!(is_gliff_source("gliff-mic-4242"));
        assert!(!is_gliff_source("gliff-capture"));
        assert!(!is_gliff_source(
            "alsa_input.pci-0000_c1_00.6.analog-stereo"
        ));
    }
}
