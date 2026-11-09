//! Privacy mode: every output but the session's own private screen is
//! covered by a black layer-shell surface above everything, which also
//! takes the pointer there. A screen plugged in while privacy mode is on is
//! covered as it appears. The surfaces belong to a Wayland connection of
//! their own, served by a thread, so they go away with it: on drop, and if
//! the server dies. The remote's own keyboards and pointers are off for as
//! long, so whoever sits there cannot type into or click the session.

use std::os::unix::process::CommandExt;

use anyhow::{Context, Result};
use wayland_client::globals::GlobalListContents;
use wayland_client::protocol::{
    wl_buffer::WlBuffer, wl_compositor::WlCompositor, wl_output, wl_output::WlOutput, wl_registry,
    wl_registry::WlRegistry, wl_shm, wl_shm::WlShm, wl_shm_pool::WlShmPool, wl_surface::WlSurface,
};
use wayland_client::{delegate_noop, Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_shell_v1::{
    Layer, ZwlrLayerShellV1,
};
use wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_surface_v1::{
    self, Anchor, KeyboardInteractivity, ZwlrLayerSurfaceV1,
};

/// The covers, kept up by their thread until drop.
pub struct Privacy {
    stop: Option<calloop::channel::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
    input: Option<InputBlock>,
}

impl Drop for Privacy {
    fn drop(&mut self) {
        drop(self.input.take());
        drop(self.stop.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct Screen {
    global: u32,
    output: WlOutput,
    name: String,
    cover: Option<Cover>,
}

struct Cover {
    surface: WlSurface,
    layer: ZwlrLayerSurfaceV1,
    buffer: Option<WlBuffer>,
}

impl Cover {
    fn destroy(self) {
        self.layer.destroy();
        self.surface.destroy();
        if let Some(buffer) = self.buffer {
            buffer.destroy();
        }
    }
}

struct State {
    conn: Connection,
    keep: String,
    compositor: WlCompositor,
    shm: WlShm,
    layer_shell: ZwlrLayerShellV1,
    screens: Vec<Screen>,
    stopped: bool,
}

impl Privacy {
    /// Cover every output except `keep`, now and as outputs appear.
    pub fn start(target: &hypr_wl::Target, keep: &str) -> Result<Self> {
        let (conn, globals, mut queue) = hypr_wl::init::<State>(target)?;
        let qh = queue.handle();
        let mut state = State {
            conn: conn.clone(),
            keep: keep.to_string(),
            compositor: globals.bind(&qh, 4..=6, ()).context("no wl_compositor")?,
            shm: globals.bind(&qh, 1..=1, ()).context("no wl_shm")?,
            layer_shell: globals
                .bind(&qh, 1..=4, ())
                .context("the compositor has no layer shell")?,
            screens: Vec::new(),
            stopped: false,
        };
        let outputs: Vec<(u32, u32)> = globals.contents().with_list(|list| {
            list.iter()
                .filter(|g| g.interface == WlOutput::interface().name)
                .map(|g| (g.name, g.version))
                .collect()
        });
        for (global, version) in outputs {
            state.add_screen(globals.registry(), global, version, &qh);
        }
        for _ in 0..3 {
            queue.roundtrip(&mut state)?;
        }
        let covered = state.screens.iter().filter(|s| s.cover.is_some()).count();
        tracing::info!(covered, "privacy mode on");
        let (stop, commands) = calloop::channel::channel();
        let thread = std::thread::Builder::new()
            .name("gliff-privacy".into())
            .spawn(move || {
                if let Err(e) = hypr_wl::run_loop(conn, queue, commands, &mut state) {
                    tracing::warn!(error = %e, "privacy covers lost the compositor");
                }
            })?;
        let input = target
            .instance()
            .map_err(anyhow::Error::from)
            .and_then(InputBlock::start)
            .map_err(
                |e| tracing::warn!(error = %format!("{e:#}"), "cannot turn the local input off"),
            )
            .ok();
        Ok(Self {
            stop: Some(stop),
            thread: Some(thread),
            input,
        })
    }
}

/// The physical input devices, turned off until drop. A guard process,
/// detached from the server, turns them back on if the server dies first.
struct InputBlock {
    instance: hypr_ipc::Instance,
    devices: Vec<String>,
    guard: std::process::Child,
}

impl InputBlock {
    fn start(instance: hypr_ipc::Instance) -> Result<Self> {
        let devices: Vec<String> = instance
            .input_devices()?
            .into_iter()
            .filter(|name| is_physical(name))
            .collect();
        let guard = std::process::Command::new(std::env::current_exe()?)
            .arg("--instance")
            .arg(&instance.signature)
            .arg("--restore-input-after")
            .arg(std::process::id().to_string())
            .args(devices.iter().flat_map(|d| ["--device", d.as_str()]))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .process_group(0)
            .spawn()
            .context("start the input guard")?;
        let mut block = Self {
            instance,
            devices: Vec::new(),
            guard,
        };
        for device in devices {
            block
                .instance
                .set_device_enabled(&device, false)
                .with_context(|| format!("turn {device} off"))?;
            block.devices.push(device);
        }
        tracing::info!(devices = ?block.devices, "local input off");
        Ok(block)
    }
}

impl Drop for InputBlock {
    fn drop(&mut self) {
        enable(&self.instance, &self.devices);
        let _ = self.guard.kill();
        let _ = self.guard.wait();
    }
}

/// Gliff's own and other programs' virtual devices carry the session's
/// input, or an input method's, and stay on.
fn is_physical(name: &str) -> bool {
    !name.starts_with("hl-virtual-")
        && !name.starts_with("virtual-")
        && !name.starts_with("unknown-device")
}

fn enable(instance: &hypr_ipc::Instance, devices: &[String]) {
    for device in devices {
        if let Err(e) = instance.set_device_enabled(device, true) {
            tracing::warn!(device, error = %e, "cannot turn the local input back on");
        }
    }
}

/// The guard's whole life: wait for the server `pid` to end, then turn
/// `devices` back on.
pub fn restore_input_after(pid: u32, instance: Option<&str>, devices: &[String]) -> Result<()> {
    let proc = std::path::PathBuf::from(format!("/proc/{pid}"));
    while proc.exists() {
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    let instance = hypr_ipc::Instance::discover(instance)?;
    enable(&instance, devices);
    Ok(())
}

impl State {
    fn add_screen(
        &mut self,
        registry: &WlRegistry,
        global: u32,
        version: u32,
        qh: &QueueHandle<Self>,
    ) {
        let output: WlOutput = registry.bind(global, version.min(4), qh, ());
        self.screens.push(Screen {
            global,
            output,
            name: String::new(),
            cover: None,
        });
    }

    fn remove_screen(&mut self, global: u32) {
        if let Some(at) = self.screens.iter().position(|s| s.global == global) {
            let screen = self.screens.remove(at);
            if let Some(cover) = screen.cover {
                cover.destroy();
            }
            screen.output.release();
        }
    }

    fn cover(&mut self, global: u32, qh: &QueueHandle<Self>) {
        let Some(screen) = self.screens.iter_mut().find(|s| s.global == global) else {
            return;
        };
        if screen.cover.is_some() || screen.name.is_empty() || screen.name == self.keep {
            return;
        }
        let surface = self.compositor.create_surface(qh, ());
        let layer = self.layer_shell.get_layer_surface(
            &surface,
            Some(&screen.output),
            Layer::Overlay,
            "gliff-privacy".into(),
            qh,
            global,
        );
        layer.set_anchor(Anchor::Top | Anchor::Bottom | Anchor::Left | Anchor::Right);
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        surface.commit();
        tracing::debug!(output = %screen.name, "covering output");
        screen.cover = Some(Cover {
            surface,
            layer,
            buffer: None,
        });
    }

    fn configure(
        &mut self,
        global: u32,
        serial: u32,
        width: u32,
        height: u32,
        qh: &QueueHandle<Self>,
    ) {
        let Some(cover) = self
            .screens
            .iter_mut()
            .find(|s| s.global == global)
            .and_then(|s| s.cover.as_mut())
        else {
            return;
        };
        cover.layer.ack_configure(serial);
        let (width, height) = (width.max(1), height.max(1));
        let black = match black_buffer(&self.shm, qh, width, height) {
            Ok(black) => black,
            Err(e) => {
                tracing::warn!(error = %e, "cannot draw a privacy cover");
                return;
            }
        };
        cover.surface.attach(Some(&black), 0, 0);
        cover
            .surface
            .damage_buffer(0, 0, width as i32, height as i32);
        cover.surface.commit();
        if let Some(old) = cover.buffer.replace(black) {
            old.destroy();
        }
    }
}

impl hypr_wl::LoopState for State {
    type Cmd = ();

    fn on_cmd(&mut self, _: ()) {}

    fn stop(&mut self) {
        for screen in &mut self.screens {
            if let Some(cover) = screen.cover.take() {
                cover.destroy();
            }
        }
        let _ = self.conn.flush();
        self.stopped = true;
    }

    fn stop_requested(&self) -> bool {
        self.stopped
    }
}

/// A buffer of `width` x `height` black pixels: a zero-filled XRGB file.
fn black_buffer(shm: &WlShm, qh: &QueueHandle<State>, width: u32, height: u32) -> Result<WlBuffer> {
    let stride = width * 4;
    let size = stride as u64 * height as u64;
    let fd = rustix::fs::memfd_create("gliff-privacy", rustix::fs::MemfdFlags::CLOEXEC)?;
    rustix::fs::ftruncate(&fd, size)?;
    let pool = shm.create_pool(std::os::fd::AsFd::as_fd(&fd), size as i32, qh, ());
    let buffer = pool.create_buffer(
        0,
        width as i32,
        height as i32,
        stride as i32,
        wl_shm::Format::Xrgb8888,
        qh,
        (),
    );
    pool.destroy();
    Ok(buffer)
}

impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(
        state: &mut Self,
        registry: &WlRegistry,
        event: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } if interface == WlOutput::interface().name => {
                state.add_screen(registry, name, version, qh);
            }
            wl_registry::Event::GlobalRemove { name } => state.remove_screen(name),
            _ => {}
        }
    }
}

impl Dispatch<WlOutput, ()> for State {
    fn event(
        state: &mut Self,
        output: &WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let Some(screen) = state.screens.iter_mut().find(|s| &s.output == output) else {
            return;
        };
        match event {
            wl_output::Event::Name { name } => screen.name = name,
            wl_output::Event::Done => {
                let global = screen.global;
                state.cover(global, qh);
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwlrLayerSurfaceV1, u32> for State {
    fn event(
        state: &mut Self,
        _: &ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        global: &u32,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_layer_surface_v1::Event::Configure {
                serial,
                width,
                height,
            } => state.configure(*global, serial, width, height, qh),
            zwlr_layer_surface_v1::Event::Closed => {
                if let Some(cover) = state
                    .screens
                    .iter_mut()
                    .find(|s| s.global == *global)
                    .and_then(|s| s.cover.take())
                {
                    cover.destroy();
                }
            }
            _ => {}
        }
    }
}

delegate_noop!(State: ignore WlCompositor);
delegate_noop!(State: ignore WlSurface);
delegate_noop!(State: ignore WlShm);
delegate_noop!(State: ignore WlShmPool);
delegate_noop!(State: ignore WlBuffer);
delegate_noop!(State: ignore ZwlrLayerShellV1);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_physical_devices_are_turned_off() {
        for name in [
            "at-translated-set-2-keyboard",
            "pixa3854:00-093a:0274-touchpad",
            "wl_keyboard",
        ] {
            assert!(is_physical(name), "{name}");
        }
        for name in [
            "hl-virtual-keyboard-gliff-server",
            "hl-virtual-keyboard-fcitx5",
            "virtual-keyboard",
            "unknown-device",
            "unknown-device-1",
        ] {
            assert!(!is_physical(name), "{name}");
        }
    }
}
