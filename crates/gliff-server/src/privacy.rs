//! Privacy mode: every output but the session's own private screen is
//! covered by a black layer-shell surface above everything, which also
//! takes the pointer there. The surfaces belong to a Wayland connection of
//! their own, so they go away with it: on drop, and if the server dies.

use anyhow::{Context, Result};
use wayland_client::globals::GlobalListContents;
use wayland_client::protocol::{
    wl_buffer::WlBuffer, wl_compositor::WlCompositor, wl_output, wl_output::WlOutput,
    wl_registry::WlRegistry, wl_shm, wl_shm::WlShm, wl_shm_pool::WlShmPool, wl_surface::WlSurface,
};
use wayland_client::{delegate_noop, Connection, Dispatch, QueueHandle};
use wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_shell_v1::{
    Layer, ZwlrLayerShellV1,
};
use wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_surface_v1::{
    self, Anchor, KeyboardInteractivity, ZwlrLayerSurfaceV1,
};

/// The covers, kept alive with their connection.
pub struct Privacy {
    _conn: Connection,
    covers: Vec<(WlSurface, ZwlrLayerSurfaceV1, Option<WlBuffer>)>,
}

impl Drop for Privacy {
    fn drop(&mut self) {
        for (surface, layer, buffer) in self.covers.drain(..) {
            layer.destroy();
            surface.destroy();
            if let Some(buffer) = buffer {
                buffer.destroy();
            }
        }
        let _ = self._conn.flush();
    }
}

#[derive(Default)]
struct State {
    outputs: hypr_wl::Outputs,
    /// Configures received, as (cover index, serial, width, height).
    configures: Vec<(usize, u32, u32, u32)>,
}

impl Privacy {
    /// Cover every output except `keep`.
    pub fn start(target: &hypr_wl::Target, keep: &str) -> Result<Self> {
        let (conn, globals, mut queue) = hypr_wl::init::<State>(target)?;
        let qh = queue.handle();
        let compositor: WlCompositor = globals.bind(&qh, 4..=6, ()).context("no wl_compositor")?;
        let shm: WlShm = globals.bind(&qh, 1..=1, ()).context("no wl_shm")?;
        let layer_shell: ZwlrLayerShellV1 = globals
            .bind(&qh, 1..=4, ())
            .context("the compositor has no layer shell")?;
        let mut state = State {
            outputs: hypr_wl::Outputs::bind(&globals, &qh)?,
            ..State::default()
        };
        queue.roundtrip(&mut state)?;
        let mut covers = Vec::new();
        for (output, info) in &state.outputs.list {
            if info.name == keep || info.name.is_empty() {
                continue;
            }
            let surface = compositor.create_surface(&qh, ());
            let layer = layer_shell.get_layer_surface(
                &surface,
                Some(output),
                Layer::Overlay,
                "gliff-privacy".into(),
                &qh,
                covers.len(),
            );
            layer.set_anchor(Anchor::Top | Anchor::Bottom | Anchor::Left | Anchor::Right);
            layer.set_exclusive_zone(-1);
            layer.set_keyboard_interactivity(KeyboardInteractivity::None);
            surface.commit();
            tracing::debug!(output = %info.name, "covering output");
            covers.push((surface, layer, None));
        }
        queue.roundtrip(&mut state)?;
        for (index, serial, width, height) in std::mem::take(&mut state.configures) {
            let Some((surface, layer, buffer)) = covers.get_mut(index) else {
                continue;
            };
            layer.ack_configure(serial);
            let black = black_buffer(&shm, &qh, width.max(1), height.max(1))?;
            surface.attach(Some(&black), 0, 0);
            surface.damage_buffer(0, 0, width as i32, height as i32);
            surface.commit();
            *buffer = Some(black);
        }
        queue.roundtrip(&mut state)?;
        tracing::info!(covered = covers.len(), "privacy mode on");
        Ok(Self {
            _conn: conn,
            covers,
        })
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
        _: &mut Self,
        _: &WlRegistry,
        _: <WlRegistry as wayland_client::Proxy>::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlOutput, ()> for State {
    fn event(
        state: &mut Self,
        output: &WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        state.outputs.handle(output, event);
    }
}

impl Dispatch<ZwlrLayerSurfaceV1, usize> for State {
    fn event(
        state: &mut Self,
        _: &ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        index: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwlr_layer_surface_v1::Event::Configure {
            serial,
            width,
            height,
        } = event
        {
            state.configures.push((*index, serial, width, height));
        }
    }
}

delegate_noop!(State: ignore WlCompositor);
delegate_noop!(State: ignore WlSurface);
delegate_noop!(State: ignore WlShm);
delegate_noop!(State: ignore WlShmPool);
delegate_noop!(State: ignore WlBuffer);
delegate_noop!(State: ignore ZwlrLayerShellV1);
