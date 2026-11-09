//! Raw access to the Hyprland IPC socket (`hyprctl` without the binary).
//!
//! Instance discovery works without `HYPRLAND_INSTANCE_SIGNATURE`, so a server
//! spawned by ssh can find the running compositor from `$XDG_RUNTIME_DIR`.

#![forbid(unsafe_code)]

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::Deserialize;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("XDG_RUNTIME_DIR is not set and /run/user/<uid> does not exist")]
    NoRuntimeDir,
    #[error("no Hyprland instance found under {0}")]
    NoInstance(PathBuf),
    #[error("Hyprland instance {0} does not exist")]
    UnknownInstance(String),
    #[error("lock file {0} has no Wayland socket name")]
    BadLockFile(PathBuf),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("hyprctl `{command}` failed: {response}")]
    Command { command: String, response: String },
}

pub type Result<T> = std::result::Result<T, Error>;

/// `$XDG_RUNTIME_DIR`, or `/run/user/<uid>` when the variable is not set and
/// that directory exists. Tailscale SSH, for one, starts commands without it.
pub fn runtime_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        return Ok(PathBuf::from(dir));
    }
    let dir = PathBuf::from(format!("/run/user/{}", rustix::process::getuid().as_raw()));
    if dir.is_dir() {
        Ok(dir)
    } else {
        Err(Error::NoRuntimeDir)
    }
}

/// A running Hyprland instance.
#[derive(Debug, Clone)]
pub struct Instance {
    pub signature: String,
    pub dir: PathBuf,
}

impl Instance {
    /// Locate an instance. Order: explicit signature, `HYPRLAND_INSTANCE_SIGNATURE`,
    /// then the newest instance under the [`runtime_dir`]'s `hypr` whose socket
    /// answers (a crashed compositor leaves its directory and socket behind).
    pub fn discover(explicit: Option<&str>) -> Result<Self> {
        let runtime_dir = runtime_dir()?;
        let from_env = std::env::var("HYPRLAND_INSTANCE_SIGNATURE").ok();
        Self::discover_in(&runtime_dir, explicit.or(from_env.as_deref()))
    }

    /// Like [`discover`](Self::discover), but reads nothing from the environment.
    pub fn discover_in(runtime_dir: &Path, explicit: Option<&str>) -> Result<Self> {
        let hypr_dir = runtime_dir.join("hypr");
        if let Some(sig) = explicit.map(str::to_owned) {
            let dir = hypr_dir.join(&sig);
            if !dir.join(".socket.sock").exists() {
                return Err(Error::UnknownInstance(sig));
            }
            return Ok(Self {
                signature: sig,
                dir,
            });
        }
        let mut candidates: Vec<(std::time::SystemTime, String, PathBuf)> = Vec::new();
        for entry in
            std::fs::read_dir(&hypr_dir).map_err(|_| Error::NoInstance(hypr_dir.clone()))?
        {
            let entry = entry?;
            let dir = entry.path();
            if !dir.join(".socket.sock").exists() {
                continue;
            }
            let modified = entry.metadata()?.modified()?;
            candidates.push((
                modified,
                entry.file_name().to_string_lossy().into_owned(),
                dir,
            ));
        }
        candidates.sort();
        candidates
            .into_iter()
            .rev()
            .find(|(_, _, dir)| UnixStream::connect(dir.join(".socket.sock")).is_ok())
            .map(|(_, signature, dir)| Self { signature, dir })
            .ok_or(Error::NoInstance(hypr_dir))
    }

    pub fn socket_path(&self) -> PathBuf {
        self.dir.join(".socket.sock")
    }

    /// Wayland socket name (e.g. `wayland-1`) as recorded in `hyprland.lock`.
    pub fn wayland_display(&self) -> Result<String> {
        let lock = self.dir.join("hyprland.lock");
        let text = std::fs::read_to_string(&lock)?;
        text.lines()
            .nth(1)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .ok_or(Error::BadLockFile(lock))
    }

    /// Send one command and return the raw response.
    pub fn request(&self, command: &str) -> Result<String> {
        let mut stream = UnixStream::connect(self.socket_path())?;
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        stream.write_all(command.as_bytes())?;
        stream.flush()?;
        let mut response = String::new();
        stream.read_to_string(&mut response)?;
        tracing::trace!(command, response = response.trim(), "hyprctl");
        Ok(response)
    }

    /// Send a command that must answer `ok`.
    pub fn dispatch(&self, command: &str) -> Result<()> {
        let response = self.request(command)?;
        if response.trim() == "ok" {
            Ok(())
        } else {
            Err(Error::Command {
                command: command.to_owned(),
                response: response.trim().to_owned(),
            })
        }
    }

    pub fn request_json<T: DeserializeOwned>(&self, command: &str) -> Result<T> {
        let response = self.request(&format!("j/{command}"))?;
        Ok(serde_json::from_str(&response)?)
    }

    pub fn monitors(&self) -> Result<Vec<Monitor>> {
        self.request_json("monitors all")
    }

    pub fn clients(&self) -> Result<Vec<Client>> {
        self.request_json("clients")
    }

    /// Pin the named workspace `workspace` to `monitor` with no gaps,
    /// border or rounding, so its one window fills the monitor. A legacy
    /// config takes the `workspace` keyword; a Lua config takes
    /// `hl.workspace_rule` through `eval`.
    pub fn bare_workspace_on(&self, workspace: &str, monitor: &str) -> Result<()> {
        let keyword = format!(
            "keyword workspace name:{workspace}, monitor:{monitor}, gapsout:0, gapsin:0, border:false, rounding:false"
        );
        match self.dispatch(&keyword) {
            Err(Error::Command { response, .. }) if response.contains("non-legacy") => {
                self.dispatch(&format!(
                    "eval hl.workspace_rule({{ workspace = \"name:{workspace}\", monitor = \"{monitor}\", gaps_in = 0, gaps_out = 0, no_border = true, no_rounding = true }})"
                ))
            }
            other => other,
        }
    }

    /// Start `command` through the compositor's shell onto the named
    /// workspace `workspace`, which takes the focus.
    pub fn exec_on_workspace(&self, workspace: &str, command: &str) -> Result<()> {
        match self.dispatch(&format!(
            "dispatch exec [workspace name:{workspace}] {command}"
        )) {
            Err(Error::Command { response, .. }) if response.contains("lua") => {
                self.dispatch(&format!(
                    "eval hl.exec_cmd({}, {{ workspace = \"name:{workspace}\" }})",
                    lua_string(command)
                ))
            }
            other => other,
        }
    }

    /// Ask the window at `address` to close, as its close button would.
    pub fn close_window(&self, address: &str) -> Result<()> {
        self.dispatcher(
            &format!("closewindow address:{address}"),
            &format!("hl.dsp.window.close({{ window = \"address:{address}\" }})"),
            || true,
        )
    }

    /// Run a dispatcher. A legacy (hyprlang) config takes `dispatch <name>
    /// <args>`; a Lua config reads `dispatch` as `hl.dispatch(...)`, rejects
    /// that form, and takes the `hl.dsp` call in `lua` instead.
    fn dispatcher(&self, legacy: &str, lua: &str, allowed: impl Fn() -> bool) -> Result<()> {
        if !allowed() {
            return Ok(());
        }
        match self.dispatch(&format!("dispatch {legacy}")) {
            Err(Error::Command { response, .. }) if response.contains("lua") => {
                if allowed() {
                    self.dispatch(&format!("dispatch {lua}"))
                } else {
                    Ok(())
                }
            }
            other => other,
        }
    }

    /// Switch one monitor's DPMS on or off, leaving the others alone.
    pub fn set_dpms(&self, monitor: &str, on: bool) -> Result<()> {
        self.set_dpms_if(monitor, on, || true)
    }

    /// Like `set_dpms`, but check cancellation before starting either IPC
    /// request. Teardown need only wait for an already-running request.
    pub fn set_dpms_if(&self, monitor: &str, on: bool, allowed: impl Fn() -> bool) -> Result<()> {
        let action = if on { "on" } else { "off" };
        self.dispatcher(
            &format!("dpms {action} {monitor}"),
            &format!("hl.dsp.dpms({{ action = \"{action}\", monitor = \"{monitor}\" }})"),
            allowed,
        )
    }

    pub fn get_option(&self, name: &str) -> Result<OptionValue> {
        self.request_json(&format!("getoption {name}"))
    }

    pub fn create_headless_output(&self, name: &str) -> Result<()> {
        self.dispatch(&format!("output create headless {name}"))
    }

    pub fn remove_output(&self, name: &str) -> Result<()> {
        self.dispatch(&format!("output remove {name}"))
    }

    /// Apply a monitor rule for `name`. A legacy (hyprlang) config takes the
    /// `monitor` keyword; a Lua config rejects keywords and takes the same
    /// rule through `eval hl.monitor`.
    pub fn set_monitor_mode(
        &self,
        name: &str,
        width: u32,
        height: u32,
        hz: u32,
        scale: f32,
    ) -> Result<()> {
        let keyword = format!("keyword monitor {name},{width}x{height}@{hz},auto,{scale}");
        match self.dispatch(&keyword) {
            Err(Error::Command { response, .. }) if response.contains("non-legacy") => {
                self.dispatch(&format!(
                    "eval hl.monitor({{ output = \"{name}\", mode = \"{width}x{height}@{hz}\", position = \"auto\", scale = {scale} }})"
                ))
            }
            other => other,
        }
    }

    /// The names of every keyboard, pointer, tablet and touch device.
    pub fn input_devices(&self) -> Result<Vec<String>> {
        let devices: Devices = self.request_json("devices")?;
        let mut names: Vec<String> = [
            devices.keyboards,
            devices.mice,
            devices.tablets,
            devices.touch,
        ]
        .into_iter()
        .flatten()
        .map(|d| d.name)
        .collect();
        names.sort();
        names.dedup();
        Ok(names)
    }

    /// Let the input device `name` reach the compositor or not.
    pub fn set_device_enabled(&self, name: &str, enabled: bool) -> Result<()> {
        let keyword = format!("keyword device[{name}]:enabled {}", u8::from(enabled));
        match self.dispatch(&keyword) {
            Err(Error::Command { response, .. }) if response.contains("non-legacy") => self
                .dispatch(&format!(
                    "eval hl.device({{ name = {name:?}, enabled = {enabled} }})"
                )),
            other => other,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct Devices {
    #[serde(default)]
    keyboards: Vec<Device>,
    #[serde(default)]
    mice: Vec<Device>,
    #[serde(default)]
    tablets: Vec<Device>,
    #[serde(default)]
    touch: Vec<Device>,
}

#[derive(Debug, Deserialize)]
struct Device {
    name: String,
}

/// Round a mode down until `scale` divides it into whole logical pixels.
/// Hyprland rejects a scale that leaves a fractional logical size ("Invalid
/// scale ... failed to find a clean divisor") and falls back to one of its
/// own, so a 1366x768 window at 1.25 would come up at scale 1. A scale of p/q
/// in lowest terms needs a mode that is a multiple of p; the result stays
/// even for the encoder. A scale with no small denominator is left alone.
pub fn clean_mode(width: u32, height: u32, scale: f32) -> (u32, u32) {
    let s = f64::from(scale);
    let Some(q) = (1..=8u32).find(|q| {
        let numerator = s * f64::from(*q);
        (numerator - numerator.round()).abs() < 1e-4
    }) else {
        return (width, height);
    };
    let p = (s * f64::from(q)).round() as u32;
    let step = if p.is_multiple_of(2) { p } else { 2 * p };
    let fit = |v: u32| (v - v % step).max(step);
    (fit(width), fit(height))
}

/// One window, as `clients` lists it.
#[derive(Debug, Clone, Deserialize)]
pub struct Client {
    pub address: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub class: String,
    pub workspace: WorkspaceRef,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WorkspaceRef {
    pub id: i64,
    pub name: String,
}

/// `text` as a Lua string literal.
fn lua_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Monitor {
    pub id: i64,
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub width: u32,
    pub height: u32,
    #[serde(default)]
    pub refresh_rate: f64,
    #[serde(default)]
    pub x: i32,
    #[serde(default)]
    pub y: i32,
    #[serde(default = "one")]
    pub scale: f32,
    #[serde(default)]
    pub transform: i32,
    #[serde(default)]
    pub focused: bool,
    #[serde(default)]
    pub disabled: bool,
    /// False while DPMS has the monitor off; Hyprland does not render it.
    #[serde(default = "yes")]
    pub dpms_status: bool,
}

fn yes() -> bool {
    true
}

fn one() -> f32 {
    1.0
}

#[derive(Debug, Clone, Deserialize)]
pub struct OptionValue {
    pub option: String,
    #[serde(default)]
    pub set: bool,
    #[serde(default)]
    pub int: Option<i64>,
    #[serde(default)]
    pub float: Option<f64>,
    #[serde(default, rename = "str")]
    pub string: Option<String>,
    #[serde(default, rename = "bool")]
    pub boolean: Option<bool>,
}

impl OptionValue {
    pub fn as_bool(&self) -> Option<bool> {
        self.boolean.or_else(|| self.int.map(|i| i != 0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovers_newest_live_instance() {
        let tmp = tempfile::tempdir().unwrap();
        let hypr = tmp.path().join("hypr");
        let make = |name: &str, age: u64| {
            let dir = hypr.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            let listener =
                std::os::unix::net::UnixListener::bind(dir.join(".socket.sock")).unwrap();
            std::fs::write(dir.join("hyprland.lock"), format!("123\nwayland-{age}\n")).unwrap();
            let t = std::time::SystemTime::now() - Duration::from_secs(age);
            std::fs::File::open(&dir).unwrap().set_modified(t).unwrap();
            listener
        };
        let _old = make("old_1_1", 20);
        let live = make("live_2_2", 10);
        // A dead instance keeps its socket file, but nothing answers it.
        drop(make("dead_3_3", 1));

        let inst = Instance::discover_in(tmp.path(), None).unwrap();
        assert_eq!(inst.signature, "live_2_2");
        assert_eq!(inst.wayland_display().unwrap(), "wayland-10");
        drop(live);
        let explicit = Instance::discover_in(tmp.path(), Some("old_1_1")).unwrap();
        assert_eq!(explicit.signature, "old_1_1");
        assert!(Instance::discover_in(tmp.path(), Some("missing")).is_err());
    }

    /// Run `call` against a socket that answers each connection with the next
    /// of `responses`, and return what it got back and the commands sent.
    fn against_socket(
        responses: &'static [&'static str],
        call: impl FnOnce(&Instance) -> Result<()>,
    ) -> (Result<()>, Vec<String>) {
        let tmp = tempfile::tempdir().unwrap();
        let inst = Instance {
            signature: "test".into(),
            dir: tmp.path().to_path_buf(),
        };
        let listener = std::os::unix::net::UnixListener::bind(inst.socket_path()).unwrap();
        let mut responses = responses.iter().copied();
        let server = std::thread::spawn(move || {
            let mut commands = Vec::new();
            loop {
                let (mut conn, _) = listener.accept().unwrap();
                let mut buf = [0; 512];
                let n = conn.read(&mut buf).unwrap();
                // An empty connection is the test hanging up.
                if n == 0 {
                    return commands;
                }
                commands.push(String::from_utf8_lossy(&buf[..n]).into_owned());
                conn.write_all(responses.next().unwrap_or("unexpected").as_bytes())
                    .unwrap();
            }
        });
        let result = call(&inst);
        drop(UnixStream::connect(inst.socket_path()).unwrap());
        (result, server.join().unwrap())
    }

    #[test]
    fn set_dpms_falls_back_to_lua_only_when_asked() {
        let (result, commands) = against_socket(&["ok"], |i| i.set_dpms("gliff-7", false));
        assert!(result.is_ok());
        assert_eq!(commands, ["dispatch dpms off gliff-7"]);

        // What a Lua config answers the legacy form with.
        const LUA: &str = "error: [string \"return hl.dispatch(dpms on gliff-7)...\"]:1: ')' expected near 'on'\n\n \u{2192} Note: dispatch in lua is a shorthand for hl.dispatch(...), your syntax might need to be updated.";
        let (result, commands) = against_socket(&[LUA, "ok"], |i| i.set_dpms("gliff-7", true));
        assert!(result.is_ok());
        assert_eq!(
            commands,
            [
                "dispatch dpms on gliff-7",
                r#"dispatch hl.dsp.dpms({ action = "on", monitor = "gliff-7" })"#,
            ]
        );

        let (result, commands) =
            against_socket(&["monitor not found"], |i| i.set_dpms("gliff-7", true));
        assert!(
            matches!(result, Err(Error::Command { response, .. }) if response == "monitor not found")
        );
        assert_eq!(commands, ["dispatch dpms on gliff-7"]);
    }

    #[test]
    fn cancelled_keep_awake_does_not_start_or_retry_ipc() {
        let (result, commands) = against_socket(&[], |i| i.set_dpms_if("gliff-7", true, || false));
        assert!(result.is_ok());
        assert!(commands.is_empty());

        let calls = std::cell::Cell::new(0);
        let (result, commands) = against_socket(&["lua syntax error"], |i| {
            i.set_dpms_if("gliff-7", true, || {
                calls.set(calls.get() + 1);
                calls.get() == 1
            })
        });
        assert!(result.is_ok());
        assert_eq!(commands, ["dispatch dpms on gliff-7"]);
    }

    #[test]
    fn input_devices_lists_every_kind_once() {
        let mut names = Vec::new();
        let (result, commands) = against_socket(
            &[
                r#"{"mice": [{"name": "touchpad"}, {"name": "unknown-device"}],
                 "keyboards": [{"name": "at-keyboard"}, {"name": "touchpad"}],
                 "tablets": [], "switches": [{"name": "Lid Switch"}]}"#,
            ],
            |i| {
                names = i.input_devices()?;
                Ok(())
            },
        );
        assert!(result.is_ok());
        assert_eq!(commands, ["j/devices"]);
        assert_eq!(names, ["at-keyboard", "touchpad", "unknown-device"]);
    }

    #[test]
    fn set_device_enabled_falls_back_to_lua() {
        let (result, commands) =
            against_socket(&["ok"], |i| i.set_device_enabled("at-keyboard", false));
        assert!(result.is_ok());
        assert_eq!(commands, ["keyword device[at-keyboard]:enabled 0"]);

        let (result, commands) = against_socket(
            &[
                "keyword can't work with non-legacy parsers. Use eval.",
                "ok",
            ],
            |i| i.set_device_enabled("at-keyboard", true),
        );
        assert!(result.is_ok());
        assert_eq!(
            commands,
            [
                "keyword device[at-keyboard]:enabled 1",
                r#"eval hl.device({ name = "at-keyboard", enabled = true })"#,
            ]
        );
    }

    #[test]
    fn parses_dpms_status() {
        let m: Monitor =
            serde_json::from_str(r#"{"id": 0, "name": "DP-2", "width": 3440, "height": 1440}"#)
                .unwrap();
        assert!(m.dpms_status, "a monitor without the field counts as on");
        let m: Monitor = serde_json::from_str(
            r#"{"id": 1, "name": "gliff-1", "width": 1280, "height": 712, "dpmsStatus": false}"#,
        )
        .unwrap();
        assert!(!m.dpms_status);
    }

    #[test]
    fn parses_option_values() {
        let v: OptionValue =
            serde_json::from_str(r#"{"option": "x", "bool": false, "set": false }"#).unwrap();
        assert_eq!(v.as_bool(), Some(false));
        let v: OptionValue =
            serde_json::from_str(r#"{"option": "x", "str": "us", "set": true }"#).unwrap();
        assert_eq!(v.string.as_deref(), Some("us"));
    }

    #[test]
    fn clean_mode_divides_into_whole_logical_pixels() {
        // Already clean: unchanged.
        assert_eq!(clean_mode(1290, 1926, 3.0), (1290, 1926));
        assert_eq!(clean_mode(2560, 1440, 2.0), (2560, 1440));
        assert_eq!(clean_mode(1920, 1080, 1.0), (1920, 1080));
        // 1.25 = 5/4: multiples of 5, kept even.
        assert_eq!(clean_mode(1366, 768, 1.25), (1360, 760));
        // 3x with an odd leftover: multiples of 6.
        assert_eq!(clean_mode(1290, 1925, 3.0), (1290, 1920));
        // 1.5 = 3/2 and 2.5 = 5/2.
        assert_eq!(clean_mode(1001, 701, 1.5), (996, 696));
        assert_eq!(clean_mode(1083, 2341, 2.5), (1080, 2340));
        // f32 rounds these scales slightly below their exact fractions.
        assert_eq!(clean_mode(1366, 768, 1.4), (1358, 756));
        assert_eq!(clean_mode(1366, 768, 1.8), (1350, 756));
        for (w, h, s) in [(1366, 768, 1.25), (1290, 1925, 3.0), (1001, 701, 1.5)] {
            let (cw, ch) = clean_mode(w, h, s);
            assert_eq!((f64::from(cw) / f64::from(s)).fract(), 0.0);
            assert_eq!((f64::from(ch) / f64::from(s)).fract(), 0.0);
            assert!(cw.is_multiple_of(2) && ch.is_multiple_of(2));
        }
    }

    const NON_LEGACY: &str = "keyword can't work with non-legacy parsers. Use eval.";
    const LUA_DISPATCH: &str = "error: [string \"return hl.dispatch(exec [workspace...\"]:1: ']' expected near 'name'\n\n Note: dispatch in lua is a shorthand for hl.dispatch(...), your syntax might need to be updated.";

    #[test]
    fn a_bare_workspace_uses_the_keyword_then_the_lua_rule() {
        let (result, commands) =
            against_socket(&["ok"], |i| i.bare_workspace_on("gliff-app-7", "gliff-7"));
        assert!(result.is_ok());
        assert_eq!(
            commands,
            ["keyword workspace name:gliff-app-7, monitor:gliff-7, gapsout:0, gapsin:0, border:false, rounding:false"]
        );
        let (result, commands) = against_socket(&[NON_LEGACY, "ok"], |i| {
            i.bare_workspace_on("gliff-app-7", "gliff-7")
        });
        assert!(result.is_ok());
        assert_eq!(
            commands[1],
            r#"eval hl.workspace_rule({ workspace = "name:gliff-app-7", monitor = "gliff-7", gaps_in = 0, gaps_out = 0, no_border = true, no_rounding = true })"#
        );
    }

    #[test]
    fn an_app_starts_through_exec_then_the_lua_call_with_its_command_quoted() {
        let (result, commands) = against_socket(&[LUA_DISPATCH, "ok"], |i| {
            i.exec_on_workspace("gliff-app-7", "foot -T \"a b\"")
        });
        assert!(result.is_ok());
        assert_eq!(
            commands,
            [
                r#"dispatch exec [workspace name:gliff-app-7] foot -T "a b""#,
                r#"eval hl.exec_cmd("foot -T \"a b\"", { workspace = "name:gliff-app-7" })"#,
            ]
        );
    }

    #[test]
    fn a_window_closes_by_address() {
        let (result, commands) = against_socket(&[LUA_DISPATCH, "ok"], |i| i.close_window("0x55"));
        assert!(result.is_ok());
        assert_eq!(
            commands,
            [
                "dispatch closewindow address:0x55",
                r#"dispatch hl.dsp.window.close({ window = "address:0x55" })"#,
            ]
        );
    }

    #[test]
    fn lua_strings_escape_quotes_backslashes_and_newlines() {
        assert_eq!(lua_string("a\"b\\c\nd"), r#""a\"b\\c\nd""#);
    }

    #[test]
    fn parses_clients_with_their_workspace() {
        let clients: Vec<Client> = serde_json::from_str(
            r#"[{"address": "0x1", "title": "t", "class": "foot", "workspace": {"id": -1337, "name": "gliff-app-7"}}]"#,
        )
        .unwrap();
        assert_eq!(clients[0].workspace.name, "gliff-app-7");
        assert_eq!(clients[0].address, "0x1");
    }
}
