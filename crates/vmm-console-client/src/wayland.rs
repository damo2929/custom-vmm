//! Wayland display for the decoded console (§7).
//!
//! The client decodes to BGRA frames; this paints them in a real window on
//! the user's compositor. It is a `FrameHandler` like `RawBgraWriter` and
//! `LatestFrame`, so it plugs into the same decode path rather than forking
//! it.
//!
//! Two deliberate choices:
//!
//! * **No libwayland.** `wayland-client`'s default backend speaks the wire
//!   protocol over the compositor socket in Rust, so the display path links
//!   no C at all — checked with `ldd`, not assumed. §1.1's preference for
//!   Rust where a production-grade crate exists applies here.
//! * **`wl_shm`, not GPU buffers.** A dmabuf path would need the client to
//!   import a surface from the host's GPU, which is a dependency the console
//!   does not otherwise have. At §7.1's 1080p a shared-memory blit is a
//!   memcpy per frame, and the frame already sits in a `Vec<u8>` on the CPU
//!   because that is where the decoder put it.

use std::os::fd::{AsFd, FromRawFd, OwnedFd};

use libvmm_core::{MediaError, VmmError, VmmResult};
use vmm_codec_sys::{PackedFormat, PackedFrame};
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_keyboard, wl_pointer, wl_registry, wl_seat, wl_shm, wl_shm_pool,
    wl_surface,
};
use wayland_client::{delegate_noop, Connection, Dispatch, EventQueue, QueueHandle, WEnum};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

/// How many shared-memory buffers to cycle through.
///
/// One is enough to be correct but stalls: the frame cannot be overwritten
/// until the compositor releases it, so the decoder would wait for a
/// compositor frame every time. Two lets the next frame be filled while the
/// current one is on screen.
const BUFFERS: usize = 2;

fn wayland_error(detail: impl Into<String>) -> VmmError {
    MediaError::Display {
        detail: detail.into(),
    }
    .into()
}

/// Absolute pointer coordinates are reported to the guest on a 0..32767
/// grid (§8.5's tablet device), independent of the window's pixel size.
const ABS_MAX: f64 = 32767.0;

/// Something the user did in the window, in the shape §8.5 sends to the VM.
///
/// The window collects these rather than sending them itself: it has no
/// control connection, and coupling the display to one would make the window
/// untestable without a live hypervisor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputEvent {
    /// EV_KEY. `code` is a Linux keycode, `value` 0 = release, 1 = press.
    Key { code: i64, value: i64 },
    /// EV_ABS, with the button state that goes with it.
    Pointer {
        x: i64,
        y: i64,
        left: bool,
        right: bool,
        middle: bool,
    },
}

/// The globals and window state the dispatch callbacks write into.
#[derive(Default)]
struct State {
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    wm_base: Option<xdg_wm_base::XdgWmBase>,
    /// Set once the compositor has acknowledged the surface; nothing may be
    /// attached before that.
    configured: bool,
    /// The user closed the window. The session stops at the next frame.
    closed: bool,
    /// Which of our buffers the compositor still owns.
    busy: [bool; BUFFERS],
    /// How many buffers the compositor has handed back. A release only
    /// happens after it has actually read the pixels, so a non-zero count is
    /// evidence the window is really on screen and not merely accepted.
    releases: u64,
    seat: Option<wl_seat::WlSeat>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>,
    /// Input the user has produced since it was last drained.
    input: Vec<InputEvent>,
    /// The surface size, needed to scale pointer motion onto the 0..32767
    /// grid. Set from the frame geometry, which is what the surface is.
    surface_size: (u32, u32),
    /// Last pointer position, so a button press without motion still carries
    /// a coordinate.
    pointer_at: (f64, f64),
    buttons: (bool, bool, bool),
    /// Keycodes currently held, so they can be released if focus is lost.
    pressed: Vec<i64>,
}

impl State {
    /// Queue the pointer's current position and button set, scaled onto the
    /// 0..32767 grid §8.5 specifies.
    fn push_pointer(&mut self) {
        let (w, h) = self.surface_size;
        if w == 0 || h == 0 {
            return; // nothing painted yet, so there is no grid to map onto
        }
        self.input.push(InputEvent::Pointer {
            x: scale_abs(self.pointer_at.0, w),
            y: scale_abs(self.pointer_at.1, h),
            left: self.buttons.0,
            right: self.buttons.1,
            middle: self.buttons.2,
        });
    }
}

/// Map a surface-local coordinate onto §8.5's absolute 0..32767 grid.
///
/// The last addressable pixel is `max - 1`, and it must land exactly on
/// `ABS_MAX` — otherwise the guest's pointer can never reach the right or
/// bottom edge, which is where the close button and the taskbar live.
pub fn scale_abs(v: f64, max: u32) -> i64 {
    if max <= 1 {
        return 0;
    }
    let last = max as f64 - 1.0;
    (v.clamp(0.0, last) / last * ABS_MAX).round() as i64
}

/// A window showing the decoded console.
pub struct WaylandWindow {
    connection: Connection,
    queue: EventQueue<State>,
    qh: QueueHandle<State>,
    state: State,
    /// Resolved at `open`, so the paint path never has to re-check it.
    shm: wl_shm::WlShm,
    surface: wl_surface::WlSurface,
    xdg_surface: xdg_surface::XdgSurface,
    toplevel: xdg_toplevel::XdgToplevel,
    pool: Option<Pool>,
    /// Which buffer to try first, rotated so the two alternate.
    next_buffer: usize,
    /// Frames actually painted, for the closing report.
    pub frames: u64,
}

/// A shared-memory pool sized for one frame geometry.
struct Pool {
    _pool: wl_shm_pool::WlShmPool,
    buffers: Vec<wl_buffer::WlBuffer>,
    /// The whole pool, mapped once.
    map: *mut u8,
    len: usize,
    width: u32,
    height: u32,
    stride: usize,
}

impl Drop for Pool {
    fn drop(&mut self) {
        for buffer in &self.buffers {
            buffer.destroy();
        }
        // SAFETY: `map`/`len` come from the mmap in `Pool::new` and are
        // unmapped exactly once, here.
        unsafe {
            libc::munmap(self.map as *mut libc::c_void, self.len);
        }
    }
}

impl WaylandWindow {
    /// Connect to the compositor named by `WAYLAND_DISPLAY` and map a window.
    ///
    /// Fails rather than falling back when there is no compositor: the caller
    /// asked for a window, and silently writing frames nowhere would look
    /// like a hung session.
    pub fn open(title: &str) -> VmmResult<Self> {
        let connection = Connection::connect_to_env().map_err(|e| {
            wayland_error(format!(
                "no Wayland compositor: {e}. WAYLAND_DISPLAY={:?}, XDG_RUNTIME_DIR={:?}",
                std::env::var("WAYLAND_DISPLAY").unwrap_or_default(),
                std::env::var("XDG_RUNTIME_DIR").unwrap_or_default(),
            ))
        })?;

        let mut queue = connection.new_event_queue();
        let qh = queue.handle();
        let display = connection.display();
        display.get_registry(&qh, ());

        let mut state = State::default();
        // One roundtrip delivers the whole global registry.
        queue
            .roundtrip(&mut state)
            .map_err(|e| wayland_error(format!("Wayland registry roundtrip: {e}")))?;

        let compositor = state
            .compositor
            .clone()
            .ok_or_else(|| wayland_error("the compositor does not advertise wl_compositor"))?;
        let wm_base = state.wm_base.clone().ok_or_else(|| {
            wayland_error(
                "the compositor does not advertise xdg_wm_base, so no window can be mapped",
            )
        })?;
        let shm = state.shm.clone().ok_or_else(|| {
            wayland_error("the compositor does not advertise wl_shm, so frames cannot be shared")
        })?;

        let surface = compositor.create_surface(&qh, ());
        let xdg_surface = wm_base.get_xdg_surface(&surface, &qh, ());
        let toplevel = xdg_surface.get_toplevel(&qh, ());
        toplevel.set_title(title.to_string());
        toplevel.set_app_id("vmm-console-client".to_string());
        // A surface must be committed with no buffer first; the compositor
        // answers with the configure this window waits for below.
        surface.commit();

        let mut window = WaylandWindow {
            connection,
            queue,
            qh,
            state,
            shm,
            surface,
            xdg_surface,
            toplevel,
            pool: None,
            next_buffer: 0,
            frames: 0,
        };

        while !window.state.configured && !window.state.closed {
            window
                .queue
                .blocking_dispatch(&mut window.state)
                .map_err(|e| wayland_error(format!("waiting for the initial configure: {e}")))?;
        }
        log::info!("Wayland window mapped on {}", display_name());
        Ok(window)
    }

    /// True once the user has closed the window.
    pub fn closed(&self) -> bool {
        self.state.closed
    }

    /// How many buffers the compositor has read and handed back.
    pub fn releases(&self) -> u64 {
        self.state.releases
    }

    /// Take everything the user has done since the last call.
    ///
    /// Pumping the queue here as well as in the paint path matters: input
    /// must keep flowing between frames, and a still console produces no
    /// frames at all.
    pub fn drain_input(&mut self) -> VmmResult<Vec<InputEvent>> {
        self.queue
            .dispatch_pending(&mut self.state)
            .map_err(|e| wayland_error(format!("Wayland dispatch: {e}")))?;
        self.connection
            .flush()
            .map_err(|e| wayland_error(format!("flushing to the compositor: {e}")))?;
        Ok(std::mem::take(&mut self.state.input))
    }

    /// Take the next free buffer, waiting for the compositor if both are
    /// still on screen.
    fn acquire(&mut self, count: usize) -> VmmResult<usize> {
        for _ in 0..240 {
            self.queue
                .dispatch_pending(&mut self.state)
                .map_err(|e| wayland_error(format!("Wayland dispatch: {e}")))?;
            if self.state.closed {
                return Err(wayland_error("the window was closed"));
            }
            for step in 0..count {
                let index = (self.next_buffer + step) % count;
                if !self.state.busy[index] {
                    self.next_buffer = (index + 1) % count;
                    return Ok(index);
                }
            }
            // Every buffer is on screen. Block until the compositor releases
            // one rather than spinning.
            self.queue
                .blocking_dispatch(&mut self.state)
                .map_err(|e| wayland_error(format!("waiting for a free buffer: {e}")))?;
        }
        Err(wayland_error(
            "the compositor released no buffer; giving up rather than blocking the session",
        ))
    }

    /// Build (or rebuild) the pool for this frame geometry.
    fn ensure_pool(&mut self, width: u32, height: u32) -> VmmResult<()> {
        if let Some(pool) = &self.pool {
            if pool.width == width && pool.height == height {
                return Ok(());
            }
            log::info!(
                "console geometry changed {}x{} -> {width}x{height}, reallocating",
                pool.width,
                pool.height
            );
        }
        // Dropping first releases the old buffers and mapping.
        self.pool = None;
        self.state.busy = [false; BUFFERS];
        self.next_buffer = 0;

        let pool = Pool::new(&self.shm, &self.qh, width, height)?;
        // Pointer coordinates are surface-local, and the surface is exactly
        // the buffer, so this is the grid motion is scaled against.
        self.state.surface_size = (width, height);
        self.toplevel
            .set_min_size(width as i32 / 4, height as i32 / 4);
        self.pool = Some(pool);
        Ok(())
    }

    /// Copy a frame in and put it on screen.
    pub fn present(&mut self, frame: &PackedFrame) -> VmmResult<()> {
        if frame.format != PackedFormat::Bgra {
            return Err(wayland_error(format!(
                "the window takes BGRA frames, got {:?}",
                frame.format
            )));
        }
        self.ensure_pool(frame.width, frame.height)?;
        let index = self.acquire(BUFFERS)?;

        {
            let pool = self
                .pool
                .as_ref()
                .ok_or_else(|| wayland_error("the frame pool went away mid-paint"))?;
            let row_bytes = frame.width as usize * 4;
            let offset = index * pool.height as usize * pool.stride;
            for row in 0..frame.height as usize {
                let src = &frame.pixels[row * frame.stride..row * frame.stride + row_bytes];
                // SAFETY: the pool is `BUFFERS * height * stride` bytes and
                // `offset + row * stride + row_bytes` stays inside it, since
                // `stride >= row_bytes` and `row < height`.
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        src.as_ptr(),
                        pool.map.add(offset + row * pool.stride),
                        row_bytes,
                    );
                }
            }
            self.state.busy[index] = true;
            self.surface.attach(Some(&pool.buffers[index]), 0, 0);
        }

        self.surface
            .damage_buffer(0, 0, frame.width as i32, frame.height as i32);
        self.surface.commit();
        self.connection
            .flush()
            .map_err(|e| wayland_error(format!("flushing to the compositor: {e}")))?;
        self.frames += 1;
        Ok(())
    }
}

impl Drop for WaylandWindow {
    fn drop(&mut self) {
        // Ordered destruction: children before parents, or the compositor
        // logs a protocol error on an orphaned role object.
        self.pool = None;
        self.toplevel.destroy();
        self.xdg_surface.destroy();
        self.surface.destroy();
    }
}

impl Pool {
    fn new(
        shm: &wl_shm::WlShm,
        qh: &QueueHandle<State>,
        width: u32,
        height: u32,
    ) -> VmmResult<Self> {
        let stride = width as usize * 4;
        let len = stride * height as usize * BUFFERS;

        // SAFETY: a plain memfd_create with a NUL-terminated literal.
        let fd = unsafe { libc::memfd_create(c"vmm-console".as_ptr(), libc::MFD_CLOEXEC) };
        if fd < 0 {
            return Err(wayland_error(format!(
                "memfd_create: {}",
                std::io::Error::last_os_error()
            )));
        }
        // SAFETY: memfd_create returned a fresh owned descriptor.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        // SAFETY: fd is a valid memfd and len is non-zero.
        if unsafe { libc::ftruncate(std::os::fd::AsRawFd::as_raw_fd(&fd), len as libc::off_t) } < 0
        {
            return Err(wayland_error(format!(
                "sizing the frame pool to {len} bytes: {}",
                std::io::Error::last_os_error()
            )));
        }
        // SAFETY: mapping the whole memfd we just sized.
        let map = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                std::os::fd::AsRawFd::as_raw_fd(&fd),
                0,
            )
        };
        if map == libc::MAP_FAILED {
            return Err(wayland_error(format!(
                "mapping the frame pool: {}",
                std::io::Error::last_os_error()
            )));
        }

        let wl_pool = shm.create_pool(fd.as_fd(), len as i32, qh, ());
        let mut buffers = Vec::with_capacity(BUFFERS);
        for index in 0..BUFFERS {
            buffers.push(wl_pool.create_buffer(
                (index * stride * height as usize) as i32,
                width as i32,
                height as i32,
                stride as i32,
                // BGRA in memory is a little-endian 0xXXRRGGBB word, which
                // is exactly Xrgb8888 — so the decoder's output blits with
                // no conversion. Xrgb rather than Argb because the console
                // is opaque and a zero alpha would show the desktop through
                // it.
                wl_shm::Format::Xrgb8888,
                qh,
                index,
            ));
        }

        Ok(Pool {
            _pool: wl_pool,
            buffers,
            map: map as *mut u8,
            len,
            width,
            height,
            stride,
        })
    }
}

fn display_name() -> String {
    std::env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| "wayland-0".to_string())
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        else {
            return;
        };
        match interface.as_str() {
            "wl_compositor" => {
                state.compositor = Some(registry.bind(name, 4, qh, ()));
            }
            "wl_shm" => {
                state.shm = Some(registry.bind(name, 1, qh, ()));
            }
            "xdg_wm_base" => {
                state.wm_base = Some(registry.bind(name, 1, qh, ()));
            }
            "wl_seat" => {
                // Version 5 is the floor for the discrete-axis and frame
                // events; the console needs neither, but binding low keeps
                // it working on minimal compositors.
                state.seat = Some(registry.bind(name, 5.min(version), qh, ()));
            }
            _ => {}
        }
    }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for State {
    fn event(
        _: &mut Self,
        wm_base: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // Answering the ping is mandatory: a compositor that gets no pong
        // treats the client as hung and may kill the window.
        if let xdg_wm_base::Event::Ping { serial } = event {
            wm_base.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for State {
    fn event(
        state: &mut Self,
        xdg_surface: &xdg_surface::XdgSurface,
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

impl Dispatch<xdg_toplevel::XdgToplevel, ()> for State {
    fn event(
        state: &mut Self,
        _: &xdg_toplevel::XdgToplevel,
        event: xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // The window manager asked for the window to go away. Record it; the
        // session ends at the next frame rather than mid-paint.
        if let xdg_toplevel::Event::Close = event {
            state.closed = true;
        }
    }
}

impl Dispatch<wl_buffer::WlBuffer, usize> for State {
    fn event(
        state: &mut Self,
        _: &wl_buffer::WlBuffer,
        event: wl_buffer::Event,
        index: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // The compositor is done reading this buffer, so it can be refilled.
        if let wl_buffer::Event::Release = event {
            state.releases += 1;
            if let Some(busy) = state.busy.get_mut(*index) {
                *busy = false;
            }
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        state: &mut Self,
        seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(caps),
        } = event
        else {
            return;
        };
        // A seat can gain or lose devices while the session runs — a laptop
        // docking, a keyboard unplugged — so this is not a one-off at start.
        if caps.contains(wl_seat::Capability::Keyboard) && state.keyboard.is_none() {
            state.keyboard = Some(seat.get_keyboard(qh, ()));
        }
        if caps.contains(wl_seat::Capability::Pointer) && state.pointer.is_none() {
            state.pointer = Some(seat.get_pointer(qh, ()));
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_keyboard::Event::Key {
                key,
                state: WEnum::Value(key_state),
                ..
            } => {
                // No translation table, and no +8. wayland.xml says of the
                // xkb_v1 keymap format: "to determine the xkb keycode,
                // clients must add 8 to the key event keycode" — so the
                // event itself carries the raw Linux evdev keycode, which is
                // exactly what §8.5's keyboard device takes. Adding 8 here
                // is the classic way to get 'a' when the user pressed
                // something else entirely.
                let code = key as i64;
                let value = match key_state {
                    wl_keyboard::KeyState::Pressed => {
                        if !state.pressed.contains(&code) {
                            state.pressed.push(code);
                        }
                        1
                    }
                    _ => {
                        state.pressed.retain(|held| *held != code);
                        0
                    }
                };
                state.input.push(InputEvent::Key { code, value });
            }
            wl_keyboard::Event::Leave { .. } => {
                // Focus left with keys still physically down. Wayland will
                // not send their release — the events go to whoever has
                // focus now — so the guest would hold them forever. Alt-Tab
                // away from a console and come back to a stuck Alt is the
                // classic form of this bug, so release them here.
                for code in state.pressed.drain(..) {
                    state.input.push(InputEvent::Key { code, value: 0 });
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Enter {
                surface_x,
                surface_y,
                ..
            }
            | wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => {
                state.pointer_at = (surface_x, surface_y);
                state.push_pointer();
            }
            wl_pointer::Event::Button {
                button,
                state: WEnum::Value(button_state),
                ..
            } => {
                let down = button_state == wl_pointer::ButtonState::Pressed;
                // Linux input-event-codes: the guest is told which buttons
                // are held, not which changed, so the set is tracked here.
                match button {
                    0x110 => state.buttons.0 = down,
                    0x111 => state.buttons.1 = down,
                    0x112 => state.buttons.2 = down,
                    _ => return,
                }
                state.push_pointer();
            }
            _ => {}
        }
    }
}

delegate_noop!(State: ignore wl_compositor::WlCompositor);
delegate_noop!(State: ignore wl_surface::WlSurface);
delegate_noop!(State: ignore wl_shm::WlShm);
delegate_noop!(State: ignore wl_shm_pool::WlShmPool);

// ---------------------------------------------------------------------------

impl crate::decode::FrameHandler for WaylandWindow {
    fn on_frame(&mut self, frame: &PackedFrame) -> VmmResult<()> {
        self.present(frame)
    }
}
