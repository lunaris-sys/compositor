/// Minimal Wayland test client for integration tests.
///
/// Connects to the compositor, creates a visible window, waits briefly
/// to ensure focus events are emitted, then exits cleanly.
///
/// Usage: WAYLAND_DISPLAY=wayland-X test-client
use std::time::Duration;
use wayland_client::{
    Connection, Dispatch, QueueHandle,
    protocol::{
        wl_compositor::WlCompositor,
        wl_registry::{self, WlRegistry},
        wl_shm::{self, WlShm},
        wl_shm_pool::WlShmPool,
        wl_surface::WlSurface,
        wl_buffer::WlBuffer,
    },
};
use wayland_protocols::xdg::shell::client::{
    xdg_surface::{self, XdgSurface},
    xdg_toplevel::{self, XdgToplevel},
    xdg_wm_base::{self, XdgWmBase},
};

struct AppState {
    compositor: Option<WlCompositor>,
    shm: Option<WlShm>,
    xdg_wm_base: Option<XdgWmBase>,
    surface: Option<WlSurface>,
    xdg_surface: Option<XdgSurface>,
    xdg_toplevel: Option<XdgToplevel>,
    configured: bool,
    done: bool,
}

impl AppState {
    fn new() -> Self {
        Self {
            compositor: None,
            shm: None,
            xdg_wm_base: None,
            surface: None,
            xdg_surface: None,
            xdg_toplevel: None,
            configured: false,
            done: false,
        }
    }
}

impl Dispatch<WlRegistry, ()> for AppState {
    fn event(
        state: &mut Self,
        registry: &WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global { name, interface, version } = event {
            match interface.as_str() {
                "wl_compositor" => {
                    state.compositor = Some(registry.bind(name, version.min(4), qh, ()));
                }
                "wl_shm" => {
                    state.shm = Some(registry.bind(name, version.min(1), qh, ()));
                }
                "xdg_wm_base" => {
                    state.xdg_wm_base = Some(registry.bind(name, version.min(2), qh, ()));
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<WlCompositor, ()> for AppState {
    fn event(_: &mut Self, _: &WlCompositor, _: wayland_client::protocol::wl_compositor::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<WlShm, ()> for AppState {
    fn event(_: &mut Self, _: &WlShm, _: wl_shm::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<WlShmPool, ()> for AppState {
    fn event(_: &mut Self, _: &WlShmPool, _: wayland_client::protocol::wl_shm_pool::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<WlBuffer, ()> for AppState {
    fn event(_: &mut Self, _: &WlBuffer, _: wayland_client::protocol::wl_buffer::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<WlSurface, ()> for AppState {
    fn event(_: &mut Self, _: &WlSurface, _: wayland_client::protocol::wl_surface::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<XdgWmBase, ()> for AppState {
    fn event(
        _state: &mut Self,
        xdg_wm_base: &XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            xdg_wm_base.pong(serial);
        }
    }
}

impl Dispatch<XdgSurface, ()> for AppState {
    fn event(
        state: &mut Self,
        xdg_surface: &XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            xdg_surface.ack_configure(serial);
            state.configured = true;
        }
    }
}

impl Dispatch<XdgToplevel, ()> for AppState {
    fn event(
        state: &mut Self,
        _: &XdgToplevel,
        event: xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_toplevel::Event::Close = event {
            state.done = true;
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let conn = Connection::connect_to_env()?;
    let mut event_queue = conn.new_event_queue();
    let qh = event_queue.handle();
    let display = conn.display();

    let mut state = AppState::new();
    let _registry = display.get_registry(&qh, ());

    // Initial roundtrip to receive globals
    event_queue.roundtrip(&mut state)?;

    let compositor = state.compositor.as_ref().expect("wl_compositor not available").clone();
    let shm = state.shm.as_ref().expect("wl_shm not available").clone();
    let xdg_wm_base = state.xdg_wm_base.as_ref().expect("xdg_wm_base not available").clone();

    // Create surface and xdg_toplevel
    let surface = compositor.create_surface(&qh, ());
    let xdg_surface = xdg_wm_base.get_xdg_surface(&surface, &qh, ());
    let xdg_toplevel = xdg_surface.get_toplevel(&qh, ());
    xdg_toplevel.set_title("lunaris-test-client".to_string());
    xdg_toplevel.set_app_id("lunaris.test-client".to_string());
    surface.commit();

    state.surface = Some(surface.clone());
    state.xdg_surface = Some(xdg_surface);
    state.xdg_toplevel = Some(xdg_toplevel);

    // Wait for configure event
    while !state.configured {
        event_queue.roundtrip(&mut state)?;
    }

    // Create a minimal 1x1 shared memory buffer and attach it
    let width = 1i32;
    let height = 1i32;
    let stride = width * 4;
    let size = (stride * height) as usize;

    let tmp = tempfile::tempfile()?;
    tmp.set_len(size as u64)?;
    let pool = shm.create_pool(
        std::os::unix::io::AsFd::as_fd(&tmp),
        size as i32,
        &qh,
        (),
    );
    let buffer = pool.create_buffer(0, width, height, stride, wl_shm::Format::Argb8888, &qh, ());

    surface.attach(Some(&buffer), 0, 0);
    surface.commit();

    // One more roundtrip to confirm the buffer is committed
    event_queue.roundtrip(&mut state)?;

    // Hold the window open briefly so the compositor emits focus events
    std::thread::sleep(Duration::from_millis(500));

    Ok(())
}
