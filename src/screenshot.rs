//! Native Wayland screen capture via `zwlr_screencopy_manager_v1`.
//!
//! The GUI uses this to take the snapshot that becomes the selection
//! overlay background, replacing the `grim` shell-out on wlroots compositors
//! (Sway, Hyprland, niri, dwl, ...). Compositors that don't implement the
//! protocol (GNOME, KDE) are handled by the `grim` fallback in [`crate::gui`].

use std::fs::File;
use std::io::Read;
use std::os::unix::io::{AsFd, FromRawFd};

use wayland_client::protocol::wl_output::WlOutput;
use wayland_client::protocol::{wl_buffer, wl_output, wl_registry, wl_shm, wl_shm_pool};
use wayland_client::{
    Connection, Dispatch, EventQueue, QueueHandle, WEnum, protocol::wl_shm::Format,
};
use wayland_protocols::xdg::xdg_output::zv1::client::{
    zxdg_output_manager_v1::{self, ZxdgOutputManagerV1},
    zxdg_output_v1::{self, ZxdgOutputV1},
};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
    zwlr_screencopy_manager_v1::{self, ZwlrScreencopyManagerV1},
};

/// Info about an output (monitor), collected from `wl_output` events.
///
/// Coordinates are global *logical* pixels — the same coordinate space
/// `wf-recorder -g` (and `slurp`) use.
#[derive(Debug, Clone)]
pub struct OutputInfo {
    /// Output name (e.g. `HDMI-A-1`); `None` if the compositor did not
    /// report one.
    pub name: Option<String>,
    /// Global logical x origin.
    pub x: i32,
    /// Global logical y origin.
    pub y: i32,
    /// Logical width.
    pub width: u32,
    /// Logical height.
    pub height: u32,
    /// Compositor scale factor (1 = 100%).
    pub scale: u32,
}

impl Default for OutputInfo {
    fn default() -> Self {
        Self {
            name: None,
            x: 0,
            y: 0,
            width: 0,
            height: 0,
            scale: 1,
        }
    }
}

/// A captured frame: 4 bytes per pixel (BGRA/ARGB/BGRX), top-down rows.
#[derive(Debug)]
pub struct Captured {
    /// Pixel format as reported by the compositor.
    pub format: Format,
    pub width: u32,
    pub height: u32,
    /// `width * height` pixels, row-major.
    pub pixels: Vec<u8>,
}

/// Convert a captured frame to straight (non-premultiplied) RGBA8.
///
/// Fails (instead of panicking) for formats this converter doesn't handle,
/// e.g. the 24-bit `Xb24`/`Xa24` some compositors offer — callers fall back
/// to `grim`.
pub fn to_rgba8(cap: &Captured) -> Result<Vec<u8>, String> {
    let src = &cap.pixels;
    let mut out = vec![0u8; src.len()];
    let (chunks, rest) = src.as_chunks::<4>();
    if !rest.is_empty() {
        return Err(format!(
            "capture format {:?} is not 4 bytes per pixel",
            cap.format
        ));
    }
    for (i, s) in chunks.iter().enumerate() {
        let o = i * 4;
        match cap.format {
            // BGRA(x) → RGBA
            Format::Bgra8888 | Format::Bgrx8888 | Format::Xrgb8888 => {
                out[o] = s[2];
                out[o + 1] = s[1];
                out[o + 2] = s[0];
                out[o + 3] = s[3];
            }
            // ARGB → RGBA
            Format::Argb8888 => {
                out[o] = s[1];
                out[o + 1] = s[2];
                out[o + 2] = s[3];
                out[o + 3] = s[0];
            }
            // ABGR → RGBA
            Format::Abgr8888 => {
                out[o] = s[3];
                out[o + 1] = s[2];
                out[o + 2] = s[1];
                out[o + 3] = s[0];
            }
            _ => return Err(format!("unsupported capture format: {:?}", cap.format)),
        }
    }
    Ok(out)
}

/// State for the capture event loop.
#[derive(Default)]
struct State {
    manager: Option<ZwlrScreencopyManagerV1>,
    xdg_manager: Option<ZxdgOutputManagerV1>,
    shm: Option<wl_shm::WlShm>,
    outputs: Vec<(WlOutput, OutputInfo)>,
    /// `(index into `outputs`, zxdg_output object)` pairs.
    xdg_outputs: Vec<(usize, ZxdgOutputV1)>,
    frame: Option<ZwlrScreencopyFrameV1>,
    frame_state: FrameState,
}

/// Progress of a single screencopy frame request.
#[derive(Debug, Default, Clone, Copy)]
struct FrameState {
    format: Option<Format>,
    width: u32,
    height: u32,
    stride: u32,
    buffer_done: bool,
    ready: bool,
    failed: bool,
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface.as_str() {
                "zwlr_screencopy_manager_v1" => {
                    state.manager = Some(registry.bind(name, version.min(3), qh, ()));
                }
                "zxdg_output_manager_v1" => {
                    state.xdg_manager = Some(registry.bind(name, version.min(3), qh, ()));
                }
                "wl_shm" => state.shm = Some(registry.bind(name, 1, qh, ())),
                "wl_output" => state.outputs.push((
                    registry.bind(name, version.min(4), qh, ()),
                    OutputInfo::default(),
                )),
                _ => {}
            }
        }
    }
}

impl Dispatch<wl_output::WlOutput, ()> for State {
    fn event(
        state: &mut Self,
        output: &wl_output::WlOutput,
        event: wl_output::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let Some((_, info)) = state.outputs.iter_mut().find(|(o, _)| o == output) else {
            return;
        };
        match event {
            // Only `x`/`y` are useful here: this protocol revision's geometry
            // event has no logical size fields — those come from
            // `zxdg_output` (see `ZxdgOutputV1` dispatch below).
            wl_output::Event::Geometry { x, y, .. } => {
                info.x = x;
                info.y = y;
            }
            wl_output::Event::Name { name } => info.name = Some(name),
            wl_output::Event::Scale { factor } => info.scale = factor.max(1) as u32,
            _ => {}
        }
    }
}

impl Dispatch<wl_shm::WlShm, ()> for State {
    fn event(
        _state: &mut Self,
        _shm: &wl_shm::WlShm,
        _event: wl_shm::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_shm_pool::WlShmPool, ()> for State {
    fn event(
        _state: &mut Self,
        _pool: &wl_shm_pool::WlShmPool,
        _event: wl_shm_pool::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_buffer::WlBuffer, ()> for State {
    fn event(
        _state: &mut Self,
        _buffer: &wl_buffer::WlBuffer,
        _event: wl_buffer::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrScreencopyManagerV1, ()> for State {
    fn event(
        _state: &mut Self,
        _manager: &ZwlrScreencopyManagerV1,
        _event: zwlr_screencopy_manager_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZxdgOutputManagerV1, ()> for State {
    fn event(
        _state: &mut Self,
        _manager: &ZxdgOutputManagerV1,
        _event: zxdg_output_manager_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZxdgOutputV1, ()> for State {
    fn event(
        state: &mut Self,
        xout: &ZxdgOutputV1,
        event: zxdg_output_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let Some((idx, _)) = state.xdg_outputs.iter().find(|(_, x)| x == xout) else {
            return;
        };
        let Some((_, info)) = state.outputs.get_mut(*idx) else {
            return;
        };
        match event {
            zxdg_output_v1::Event::LogicalPosition { x, y } => {
                info.x = x;
                info.y = y;
            }
            zxdg_output_v1::Event::LogicalSize { width, height } => {
                info.width = width.max(0) as u32;
                info.height = height.max(0) as u32;
            }
            zxdg_output_v1::Event::Name { name } => info.name = Some(name),
            _ => {}
        }
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, ()> for State {
    fn event(
        state: &mut Self,
        _frame: &ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_screencopy_frame_v1::Event::Buffer {
                format: WEnum::Value(fmt),
                width,
                height,
                stride,
            } => {
                // The `buffer` event may repeat; the first one wins.
                if state.frame_state.format.is_none() {
                    state.frame_state.format = Some(fmt);
                    state.frame_state.width = width;
                    state.frame_state.height = height;
                    state.frame_state.stride = stride;
                }
            }
            zwlr_screencopy_frame_v1::Event::BufferDone => state.frame_state.buffer_done = true,
            zwlr_screencopy_frame_v1::Event::Ready { .. } => state.frame_state.ready = true,
            zwlr_screencopy_frame_v1::Event::Failed => state.frame_state.failed = true,
            _ => {}
        }
    }
}

/// A live screencopy session.
pub struct Capture {
    queue: EventQueue<State>,
    state: State,
}

impl Capture {
    /// Connect to the compositor and bind the required globals.
    pub fn new() -> Result<Self, String> {
        let conn = Connection::connect_to_env().map_err(|e| e.to_string())?;
        let mut queue = conn.new_event_queue::<State>();
        let qh = queue.handle();
        let mut state = State::default();
        conn.display().get_registry(&qh, ());
        queue.roundtrip(&mut state).map_err(|e| e.to_string())?;
        if state.manager.is_none() {
            return Err("compositor does not provide zwlr_screencopy_manager_v1".to_owned());
        }
        if state.shm.is_none() {
            return Err("compositor does not provide wl_shm".to_owned());
        }

        // Request `zxdg_output` for every output: `wl_output` geometry only
        // gives millimeter dimensions, while the screencopy/`wf-recorder`
        // coordinate space is logical pixels, which `zxdg_output` reports.
        if let Some(xdg_manager) = state.xdg_manager.clone() {
            let qh = queue.handle();
            for (i, (output, _)) in state.outputs.iter().enumerate() {
                let xout = xdg_manager.get_xdg_output(output, &qh, ());
                state.xdg_outputs.push((i, xout));
            }
            queue.roundtrip(&mut state).map_err(|e| e.to_string())?;
        }

        Ok(Self { queue, state })
    }

    /// The outputs the compositor reports, with global logical geometry.
    pub fn outputs(&self) -> &[(WlOutput, OutputInfo)] {
        &self.state.outputs
    }

    /// Capture `output` as raw pixels.
    #[cfg_attr(not(feature = "gui"), allow(dead_code))]
    pub fn capture_output(
        &mut self,
        output: &WlOutput,
        include_cursor: bool,
    ) -> Result<Captured, String> {
        self.capture(output, include_cursor, None)
    }

    /// Capture a region of `output`, in *output-local logical* coordinates.
    /// The compositor clips the region to the output's extents.
    pub fn capture_output_region(
        &mut self,
        output: &WlOutput,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        include_cursor: bool,
    ) -> Result<Captured, String> {
        self.capture(output, include_cursor, Some((x, y, width, height)))
    }

    fn capture(
        &mut self,
        output: &WlOutput,
        include_cursor: bool,
        region: Option<(i32, i32, i32, i32)>,
    ) -> Result<Captured, String> {
        let qh = self.queue.handle();
        let manager = self.state.manager.as_ref().ok_or("no screencopy manager")?;
        let cursor = if include_cursor { 1 } else { 0 };
        let frame = match region {
            Some((x, y, w, h)) => {
                manager.capture_output_region(cursor, output, x, y, w, h, &qh, ())
            }
            None => manager.capture_output(cursor, output, &qh, ()),
        };
        self.state.frame = Some(frame.clone());
        self.state.frame_state = FrameState::default();

        // Phase 1: wait for the buffer info and `buffer_done`.
        loop {
            self.queue
                .blocking_dispatch(&mut self.state)
                .map_err(|e| e.to_string())?;
            let st = self.state.frame_state;
            if st.failed {
                frame.destroy();
                self.state.frame = None;
                return Err("capture failed: compositor refused the request".to_owned());
            }
            if st.buffer_done && st.format.is_some() {
                break;
            }
        }
        let st = self.state.frame_state;
        let (width, height, stride) = (st.width, st.height, st.stride);
        let format = st.format.unwrap();
        let size = (stride * height) as usize;

        // Allocate the shared memory the compositor writes into.
        let mut file = shm_file(size).map_err(|e| e.to_string())?;
        let shm = self.state.shm.as_ref().ok_or("no shm")?;
        let pool = shm.create_pool(file.as_fd(), size as i32, &qh, ());
        let buffer = pool.create_buffer(
            0,
            width as i32,
            height as i32,
            stride as i32,
            format,
            &qh,
            (),
        );
        pool.destroy();
        frame.copy(&buffer);

        // Phase 2: wait for `ready` (pixels are in the shared memory).
        loop {
            self.queue
                .blocking_dispatch(&mut self.state)
                .map_err(|e| e.to_string())?;
            let st = self.state.frame_state;
            if st.failed {
                frame.destroy();
                self.state.frame = None;
                return Err("capture failed: compositor refused the request".to_owned());
            }
            if st.ready {
                break;
            }
        }
        frame.destroy();
        buffer.destroy();

        // The compositor has written the pixels into the memfd — read them.
        let mut pixels = vec![0u8; size];
        file.read_exact(&mut pixels).map_err(|e| e.to_string())?;

        Ok(Captured {
            format,
            width,
            height,
            pixels,
        })
    }
}

/// Create an anonymous `memfd` of `size` bytes for shared memory.
fn shm_file(size: usize) -> std::io::Result<File> {
    let name = b"wlgif-shot\0";
    // SAFETY: `memfd_create` returns a fresh, owned fd on success.
    let fd = unsafe {
        libc::syscall(
            libc::SYS_memfd_create,
            name.as_ptr() as libc::c_long,
            libc::MFD_CLOEXEC as libc::c_long,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `fd` is a valid, owned file descriptor.
    let file = unsafe { File::from_raw_fd(fd as libc::c_int) };
    file.set_len(size as u64)?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bgra_to_rgba() {
        let cap = Captured {
            format: Format::Bgra8888,
            width: 2,
            height: 1,
            pixels: vec![1, 2, 3, 255, 4, 5, 6, 255],
        };
        assert_eq!(to_rgba8(&cap).unwrap(), vec![3, 2, 1, 255, 6, 5, 4, 255]);
    }

    #[test]
    fn xrgb_to_rgba() {
        let cap = Captured {
            format: Format::Xrgb8888,
            width: 1,
            height: 1,
            pixels: vec![1, 2, 3, 255],
        };
        assert_eq!(to_rgba8(&cap).unwrap(), vec![3, 2, 1, 255]);
    }

    #[test]
    fn argb_to_rgba() {
        let cap = Captured {
            format: Format::Argb8888,
            width: 1,
            height: 1,
            pixels: vec![255, 1, 2, 3],
        };
        assert_eq!(to_rgba8(&cap).unwrap(), vec![1, 2, 3, 255]);
    }

    #[test]
    fn unsupported_format_is_an_error() {
        // 24-bit format: 3 bytes per pixel, not convertible here.
        let cap = Captured {
            format: Format::Rgb888,
            width: 1,
            height: 1,
            pixels: vec![1, 2, 3],
        };
        assert!(to_rgba8(&cap).is_err());
    }
}
