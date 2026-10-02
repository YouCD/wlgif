//! Optional egui frontend: pick a region on a screen snapshot, tune
//! parameters, record in a background thread, and preview the resulting GIF.
//!
//! Reuses the same `backend`/`converter` modules as the CLI.
//!
//! Region selection works by capturing the screen first, then making the
//! main window fullscreen and showing that snapshot in it. A *separate*
//! overlay window doesn't work on tiling WMs like niri — it opens as just
//! another tile. The capture prefers the native Wayland screencopy protocol
//! (see [`crate::screenshot`]) and falls back to `grim`.

use crate::backend::{self, RecordConfig};
use crate::converter;
use crate::error::Error;
use crate::region::Region;
use crate::screenshot;
use anyhow::{Context, Result};
use egui::epaint::text::{FontData, FontDefinitions, FontFamily};
use egui::load::SizedTexture;
use egui::{
    Align2, Color32, ColorImage, FontId, Key, PointerButton, Pos2, Rect, RichText, Stroke,
    StrokeKind, TextureOptions, ViewportCommand,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::Instant;
use wayland_client::protocol::wl_output::WlOutput;

/// Minimum drag size (px) before a selection is accepted.
const MIN_DRAG: f32 = 8.0;

pub fn run(output_default: &Path) -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("wlgif")
            .with_inner_size([480.0, 440.0]),
        ..Default::default()
    };

    let output_default = output_default.to_path_buf();
    eframe::run_native(
        "wlgif",
        options,
        Box::new(move |cc| Ok(Box::new(WlgifApp::new(cc, output_default)))),
    )
}

/// Output format for a capture job.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Gif,
    Video,
    Shot,
}

impl Mode {
    /// Expected file extension for this mode.
    fn ext(self) -> &'static str {
        match self {
            Mode::Gif => "gif",
            Mode::Video => "mp4",
            Mode::Shot => "png",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Idle,
    Recording,
    Converting,
    Done,
    Error,
}

#[derive(Clone)]
struct JobState {
    phase: Phase,
    started: Instant,
    duration: f64,
    /// Conversion progress in 0..=1 (meaningful in the Converting phase).
    progress: f64,
    message: String,
    output: Option<PathBuf>,
}

impl JobState {
    fn idle() -> Self {
        Self {
            phase: Phase::Idle,
            started: Instant::now(),
            duration: 0.0,
            progress: 0.0,
            message: String::new(),
            output: None,
        }
    }
}

struct WlgifApp {
    selecting: bool,
    /// Whether the window is currently in selection-fullscreen.
    overlay_active: bool,
    /// Decoded screenshot pixels: (width, height, RGBA bytes). Shown as the
    /// overlay background so the user can see what they are selecting.
    screenshot_rgba: Option<(usize, usize, Vec<u8>)>,
    /// How the captured image maps to global screen coordinates.
    shot_info: ShotInfo,
    /// The screenshot uploaded as a texture. The `TextureHandle` MUST be kept
    /// alive: egui frees a texture as soon as the last handle is dropped.
    overlay_texture: Option<egui::TextureHandle>,
    notice: Option<String>,
    region: Option<Region>,
    geometry: String,
    /// Output format: GIF (record + convert) or video (record only).
    mode: Mode,
    fps: f64,
    duration: f64,
    width: f64,
    optimize: bool,
    backend: String,
    output: String,
    job: Arc<Mutex<JobState>>,
    stop: Arc<AtomicBool>,
    /// Decoded preview GIF: frames as (w, h, RGBA bytes) plus per-frame delays
    /// in seconds. egui ships no GIF decoder, so we animate the frames
    /// ourselves (see `decode_gif_preview`).
    preview_frames: Vec<(u32, u32, Vec<u8>)>,
    preview_delays: Vec<f32>,
    /// Index of the frame currently shown.
    preview_idx: usize,
    /// `ctx` time when the current frame was last shown.
    preview_shown_at: f64,
    /// Texture of the currently shown frame (reloaded on frame changes).
    preview_tex: Option<egui::TextureHandle>,
    /// Index of the frame `preview_tex` currently holds.
    preview_tex_idx: usize,
    drag_start: Option<Pos2>,
    drag_cur: Option<Pos2>,
}

impl WlgifApp {
    fn new(cc: &eframe::CreationContext<'_>, output_default: PathBuf) -> Self {
        setup_fonts(cc);

        Self {
            selecting: false,
            overlay_active: false,
            screenshot_rgba: None,
            shot_info: ShotInfo::default(),
            overlay_texture: None,
            notice: None,
            region: None,
            geometry: String::new(),
            mode: match output_default.extension().and_then(|e| e.to_str()) {
                Some("mp4") => Mode::Video,
                Some("png") => Mode::Shot,
                _ => Mode::Gif,
            },
            fps: 15.0,
            duration: 5.0,
            width: 0.0,
            optimize: true,
            backend: "auto".to_owned(),
            output: output_default.to_string_lossy().into_owned(),
            job: Arc::new(Mutex::new(JobState::idle())),
            stop: Arc::new(AtomicBool::new(false)),
            preview_frames: Vec::new(),
            preview_delays: Vec::new(),
            preview_idx: 0,
            preview_shown_at: f64::NEG_INFINITY,
            preview_tex: None,
            preview_tex_idx: 0,
            drag_start: None,
            drag_cur: None,
        }
    }

    fn region_label(&self) -> String {
        match self.region {
            Some(r) => format!("{}×{} @ ({}, {})", r.width, r.height, r.x, r.y),
            None => "未设置".to_owned(),
        }
    }

    /// Decode a PNG into a single preview frame.
    fn decode_png_preview(bytes: &[u8]) -> Option<(Vec<(u32, u32, Vec<u8>)>, Vec<f32>)> {
        let (w, h, rgba) = decode_png_rgba(bytes.to_vec()).ok()?;
        Some((vec![(w as u32, h as u32, rgba)], vec![f32::MAX]))
    }

    /// Decode a GIF into raw RGBA frames with their delays (seconds), for the
    /// in-app preview. Returns `None` if the file is not a decodable GIF.
    fn decode_gif_preview(bytes: &[u8]) -> Option<(Vec<(u32, u32, Vec<u8>)>, Vec<f32>)> {
        use image::AnimationDecoder;
        let decoder = image::codecs::gif::GifDecoder::new(std::io::Cursor::new(bytes)).ok()?;
        let frames = decoder.into_frames().collect_frames().ok()?;
        if frames.is_empty() {
            return None;
        }
        let mut out = Vec::with_capacity(frames.len());
        let mut delays = Vec::with_capacity(frames.len());
        for frame in frames {
            let (num, den) = frame.delay().numer_denom_ms();
            // A delay of 0 is undefined per the GIF spec; 100 ms is the
            // conventional default most decoders use.
            let ms = if num == 0 { 10.0 } else { num as f32 / den.max(1) as f32 };
            delays.push(ms / 1000.0);
            let buf = frame.buffer();
            out.push((buf.width(), buf.height(), buf.as_raw().clone()));
        }
        Some((out, delays))
    }

    fn set_region(&mut self, region: Option<Region>) {
        self.region = region;
        self.geometry = match region {
            Some(r) => format!("{}x{}+{}+{}", r.width, r.height, r.x, r.y),
            None => String::new(),
        };
    }

    /// Map a drag rect (in fullscreen-window coordinates) to global logical
    /// screen coordinates — the coordinate space `wf-recorder -g` (and
    /// `slurp`) use.
    ///
    /// While the window is fullscreen it covers the whole captured output,
    /// so window coordinates are 1:1 with the output's logical coordinates
    /// (the ratio only corrects sub-pixel rounding between the window size
    /// and the output's logical size); the origin shifts to the output's
    /// global position for multi-monitor setups.
    fn drag_to_region(drag: Rect, window: Rect, info: ShotInfo) -> Region {
        let sx = info.out_w / window.width().max(1.0);
        let sy = info.out_h / window.height().max(1.0);
        let x0 = (info.origin_x as f32 + drag.left() * sx).round().max(0.0);
        let y0 = (info.origin_y as f32 + drag.top() * sy).round().max(0.0);
        let x1 = (info.origin_x as f32 + drag.right() * sx).round();
        let y1 = (info.origin_y as f32 + drag.bottom() * sy).round();
        let xmax = info.origin_x as f32 + info.out_w;
        let ymax = info.origin_y as f32 + info.out_h;
        let (x, y) = (x0.min(x1), y0.min(y1));
        let (x1, y1) = (x1.max(x0).min(xmax), y1.max(y0).min(ymax));
        Region {
            x: x as u32,
            y: y as u32,
            width: (x1 - x).max(1.0) as u32,
            height: (y1 - y).max(1.0) as u32,
        }
    }

    /// Accept the finished drag rect as the region. In screenshot mode the
    /// capture happens immediately on release — no extra click needed.
    fn finish_selection(&mut self, drag: Rect, window: Rect) {
        if drag.width() > MIN_DRAG && drag.height() > MIN_DRAG {
            self.set_region(Some(Self::drag_to_region(drag, window, self.shot_info)));
            if self.mode == Mode::Shot {
                self.start_recording();
            }
        }
    }

    /// Add a slider inside a fixed-width cell so two fit on one row. egui
    /// sliders have no width setter — they fill whatever rect they are
    /// allocated, so hand the inner `Ui` a sized rect first.
    fn slider_cell(
        ui: &mut egui::Ui,
        enabled: bool,
        width: f32,
        slider: egui::Slider<'_>,
    ) -> egui::Response {
        let size = egui::vec2(width, ui.spacing().interact_size.y);
        ui.allocate_ui_with_layout(size, egui::Layout::left_to_right(egui::Align::Center), |
            inner,
        | {
            inner.add_enabled(enabled, slider)
        })
        .inner
    }

    /// Snapshot the screen and open the selection overlay.
    fn start_selection(&mut self, frame: &eframe::Frame) {
        self.notice = None;
        self.drag_start = None;
        self.drag_cur = None;
        self.overlay_texture = None;
        match capture_screen(frame) {
            Ok(shot) => {
                self.screenshot_rgba = Some((shot.width, shot.height, shot.rgba));
                self.shot_info = shot.info;
                self.selecting = true;
            }
            Err(e) => {
                self.notice = Some(format!("屏幕截图失败: {e}"));
            }
        }
    }

    /// Handle pointer input and paint the selection overlay in the
    /// (fullscreen) main window.
    fn update_overlay(&mut self, ui: &mut egui::Ui) {
        let pressed = ui
            .ctx()
            .input(|i| i.pointer.button_pressed(PointerButton::Primary));
        let down = ui
            .ctx()
            .input(|i| i.pointer.button_down(PointerButton::Primary));
        let released = ui
            .ctx()
            .input(|i| i.pointer.button_released(PointerButton::Primary));
        let pos = ui.ctx().input(|i| i.pointer.interact_pos());

        if pressed && let Some(pos) = pos {
            self.drag_start = Some(pos);
            self.drag_cur = Some(pos);
        }
        if down {
            self.drag_cur = pos.or(self.drag_cur);
        }
        if released {
            if let (Some(a), Some(b)) = (self.drag_start, self.drag_cur) {
                self.finish_selection(
                    Rect::from_two_pos(a, b),
                    ui.ctx().input(|i| i.raw.screen_rect).unwrap_or(ui.max_rect()),
                );
            }
            self.selecting = false;
            ui.ctx().request_repaint();
            return;
        }
        if ui.ctx().input(|i| i.key_pressed(Key::Escape)) {
            self.selecting = false;
            ui.ctx().request_repaint();
            return;
        }

        // The window is fullscreen on the captured output, so the whole
        // window maps 1:1 (modulo rounding) to the captured image. Use the
        // full window rect rather than this panel's: egui's CentralPanel
        // insets its content by a margin, and we want the screenshot to
        // cover every pixel.
        let screen = ui.ctx().input(|i| i.raw.screen_rect).unwrap_or(ui.max_rect());

        // Screenshot background, uploaded once as a texture. The handle is
        // stored in the struct: egui frees a texture when the last
        // `TextureHandle` is dropped.
        if let Some((w, h, rgba)) = &self.screenshot_rgba {
            if self.overlay_texture.is_none() {
                let color_image = ColorImage::from_rgba_unmultiplied([*w, *h], rgba);
                let handle = ui.ctx().load_texture(
                    "wlgif-overlay",
                    color_image,
                    TextureOptions::default(),
                );
                self.overlay_texture = Some(handle);
            }
            if let Some(handle) = &self.overlay_texture {
                let tex = SizedTexture::from_handle(handle);
                let uv = Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(1.0, 1.0));
                ui.painter().image(tex.id, screen, uv, Color32::WHITE);
            }
        }

        // Dim it and draw the live selection rectangle.
        let painter = ui.painter();
        painter.rect_filled(screen, 0.0, Color32::from_rgba_unmultiplied(0, 0, 0, 110));

        if let (Some(a), Some(b)) = (self.drag_start, self.drag_cur) {
            let rect = Rect::from_two_pos(a, b);
            painter.rect_filled(
                rect,
                0.0,
                Color32::from_rgba_unmultiplied(255, 255, 255, 20),
            );
            painter.rect_stroke(
                rect,
                0.0,
                Stroke::new(2.0, Color32::from_rgb(80, 170, 255)),
                StrokeKind::Outside,
            );
            let label = format!("{:.0}×{:.0}", rect.width(), rect.height());
            painter.text(
                rect.left_top() + egui::vec2(8.0, 20.0),
                Align2::LEFT_TOP,
                label,
                FontId::monospace(15.0),
                Color32::WHITE,
            );
        } else {
            painter.text(
                screen.center(),
                Align2::CENTER_CENTER,
                "拖拽选择区域 · Esc 取消",
                FontId::monospace(18.0),
                Color32::WHITE,
            );
        }

        ui.ctx().request_repaint();
    }

    fn show_status(&self, ui: &mut egui::Ui) {
        let job = self.job.lock().unwrap();
        match job.phase {
            Phase::Idle => {}
            Phase::Recording => {
                let elapsed = job.started.elapsed().as_secs_f64();
                let progress = if job.duration > 0.0 {
                    (elapsed / job.duration).min(1.0)
                } else {
                    0.5
                };
                let text = if job.duration > 0.0 {
                    format!("录制中 · {:.1}s / {:.0}s", elapsed, job.duration)
                } else {
                    format!("录制中 · {:.1}s", elapsed)
                };
                let mut bar = egui::ProgressBar::new(progress as f32).text(text);
                if job.duration == 0.0 {
                    bar = bar.animate(true);
                }
                ui.add(bar);
            }
            Phase::Converting => {
                let mut bar =
                    egui::ProgressBar::new((job.progress).clamp(0.0, 1.0) as f32)
                        .text("转换为 GIF…");
                // No frame count available (ffprobe failed): fall back to an
                // indeterminate animated bar.
                if job.progress <= 0.0 {
                    bar = bar.animate(true);
                }
                ui.add(bar);
            }
            Phase::Done => {
                ui.label(
                    RichText::new(format!("✓ {}", job.message))
                        .color(Color32::from_rgb(120, 220, 120)),
                );
            }
            Phase::Error => {
                ui.label(
                    RichText::new(format!("✗ {}", job.message))
                        .color(Color32::from_rgb(255, 90, 90)),
                );
            }
        }
    }

    fn start_recording(&mut self) {
        let output = PathBuf::from(self.output.trim());
        let want_ext = self.mode.ext();
        if output.extension().and_then(|e| e.to_str()) != Some(want_ext) {
            *self.job.lock().unwrap() = JobState {
                phase: Phase::Error,
                started: Instant::now(),
                duration: 0.0,
                progress: 0.0,
                message: format!("输出文件必须是 .{}", want_ext),
                output: None,
            };
            return;
        }

        let region = self.region;
        let mode = self.mode;
        let fps = self.fps.round() as u32;
        let duration = self.duration as f32;
        let width = if self.width > 0.5 {
            Some(self.width as u32)
        } else {
            None
        };
        let optimize = self.optimize;
        let backend_name = if self.backend == "auto" {
            None
        } else {
            Some(self.backend.clone())
        };

        self.stop.store(false, Ordering::SeqCst);
        self.notice = None;
        self.preview_frames.clear();
        self.preview_delays.clear();
        self.preview_idx = 0;
        self.preview_shown_at = f64::NEG_INFINITY;
        self.preview_tex = None;
        self.preview_tex_idx = 0;
        *self.job.lock().unwrap() = JobState {
            phase: Phase::Recording,
            started: Instant::now(),
            duration: self.duration,
            progress: 0.0,
            message: String::new(),
            output: None,
        };

        let params = CaptureJob {
            mode,
            region,
            fps,
            duration,
            width,
            optimize,
            output: output.clone(),
            backend_name,
        };

        let job = Arc::clone(&self.job);
        let stop = Arc::clone(&self.stop);
        std::thread::spawn(move || {
            let result = record_and_convert(&params, &stop, &job);
            let state = match result {
                Ok(()) => JobState {
                    phase: Phase::Done,
                    started: Instant::now(),
                    duration: 0.0,
                    progress: 0.0,
                    message: format!("已保存 {}", params.output.display()),
                    output: Some(params.output),
                },
                Err(e) => JobState {
                    phase: Phase::Error,
                    started: Instant::now(),
                    duration: 0.0,
                    progress: 0.0,
                    message: e.to_string(),
                    output: None,
                },
            };
            *job.lock().unwrap() = state;
        });
    }
}

impl eframe::App for WlgifApp {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        if self.selecting {
            // Make *this* window cover its output. A separate overlay window
            // opens as just another tile on tiling WMs (niri), so it can
            // neither cover the screen nor receive drags over it.
            if !self.overlay_active {
                ui.ctx()
                    .send_viewport_cmd(ViewportCommand::Fullscreen(true));
                self.overlay_active = true;
            }
            egui::CentralPanel::default().show(ui, |ui| self.update_overlay(ui));
            return;
        }
        if self.overlay_active {
            // Selection finished or cancelled: leave fullscreen.
            ui.ctx()
                .send_viewport_cmd(ViewportCommand::Fullscreen(false));
            self.overlay_active = false;
        }

        egui::CentralPanel::default().show(ui, |ui| {
            ui.heading("wlgif");
            ui.label(RichText::new("将 Wayland 屏幕区域录制成 GIF、视频或截图").weak());
            ui.separator();

            // Output format
            ui.horizontal(|ui| {
                ui.label(RichText::new("格式").weak());
                let gif = ui.radio_value(&mut self.mode, Mode::Gif, "GIF");
                let video = ui.radio_value(&mut self.mode, Mode::Video, "视频");
                let shot = ui.radio_value(&mut self.mode, Mode::Shot, "截屏");
                // Keep the output extension in sync with the format.
                if gif.changed() || video.changed() || shot.changed() {
                    let p = PathBuf::from(self.output.trim());
                    if let Some(ext) = p.extension().and_then(|e| e.to_str())
                        && ext != self.mode.ext()
                        && let Some(stem) = p.file_stem()
                    {
                        self.output =
                            format!("{}.{}", stem.to_string_lossy(), self.mode.ext());
                    }
                }
            });

            ui.separator();

            // Region selection
            ui.horizontal(|ui| {
                if ui.button("选择区域…").clicked() {
                    self.start_selection(frame);
                }
                if self.region.is_some() && ui.button("清除").clicked() {
                    self.set_region(None);
                }
            });
            ui.horizontal(|ui| {
                ui.label(RichText::new("区域").weak());
                ui.label(self.region_label());
                if ui
                    .add(egui::TextEdit::singleline(&mut self.geometry).desired_width(150.0))
                    .changed()
                    && !self.geometry.trim().is_empty()
                    && let Ok(r) = Region::from_geometry(self.geometry.trim())
                {
                    self.set_region(Some(r));
                }
            });

            ui.separator();

            // Parameters — two controls per row to save vertical space.
            let timed = self.mode != Mode::Shot;
            ui.horizontal(|ui| {
                Self::slider_cell(
                    ui,
                    timed,
                    170.0,
                    egui::Slider::new(&mut self.fps, 1.0..=60.0).text("fps"),
                );
                Self::slider_cell(
                    ui,
                    timed,
                    200.0,
                    egui::Slider::new(&mut self.duration, 0.0..=300.0).text("时长（秒）"),
                );
            });
            ui.horizontal(|ui| {
                // Width and palette optimization only apply to GIF conversion.
                Self::slider_cell(
                    ui,
                    self.mode == Mode::Gif,
                    200.0,
                    egui::Slider::new(&mut self.width, 0.0..=1920.0).text("缩放宽度"),
                );
                ui.add_enabled(
                    self.mode == Mode::Gif,
                    egui::Checkbox::new(&mut self.optimize, "调色板优化"),
                );
            });
            ui.label(
                RichText::new("时长 0 = 手动停止 · 缩放宽度 0 = 原始")
                    .weak()
                    .small(),
            );

            egui::ComboBox::from_label("后端")
                .selected_text(&self.backend)
                .show_ui(ui, |ui| {
                    for name in ["auto", "wlroots", "xdg-desktop-portal"] {
                        ui.selectable_value(&mut self.backend, name.to_owned(), name);
                    }
                });

            ui.horizontal(|ui| {
                ui.label(RichText::new("输出").weak());
                ui.add(egui::TextEdit::singleline(&mut self.output).desired_width(f32::INFINITY));
            });

            ui.separator();

            // Actions
            let phase = self.job.lock().unwrap().phase;
            let busy = matches!(phase, Phase::Recording | Phase::Converting);
            ui.horizontal(|ui| {
                // Recording may start whenever nothing is running — including
                // after a finished job, which keeps the phase at Done to show
                // its result and preview.
                let record_label = match self.mode {
                    Mode::Shot => "●  截取",
                    _ => "●  录制",
                };
                let record = egui::Button::new(RichText::new(record_label).strong());
                if ui.add_enabled(!busy, record).clicked() {
                    self.start_recording();
                }
                // The stop flag is only honoured while recording, so the button
                // only exists then (during conversion there is nothing to stop).
                if phase == Phase::Recording && ui.button("■  停止").clicked() {
                    self.stop.store(true, Ordering::SeqCst);
                }
            });

            self.show_status(ui);

            if let Some(notice) = &self.notice {
                ui.label(
                    RichText::new(format!("✗ {notice}")).color(Color32::from_rgb(255, 90, 90)),
                );
            }

            // Animated GIF preview. egui has no image loader, so decode with
            // the `image` crate and advance frames by their stored delays.
            let (phase, output) = {
                let job = self.job.lock().unwrap();
                (job.phase, job.output.clone())
            };
            if phase == Phase::Done
                && self.preview_frames.is_empty()
                && let Some(path) = output
                && let Ok(bytes) = fs::read(&path)
                && let Some((frames, delays)) = match path.extension().and_then(|e| e.to_str()) {
                    Some("png") => Self::decode_png_preview(&bytes),
                    _ => Self::decode_gif_preview(&bytes),
                }
            {
                self.preview_frames = frames;
                self.preview_delays = delays;
                self.preview_idx = 0;
                self.preview_shown_at = f64::NEG_INFINITY;
                self.preview_tex = None;
            }
            if !self.preview_frames.is_empty() {
                let now = ui.ctx().input(|i| i.time);
                let delays = &self.preview_delays;
                if now - self.preview_shown_at >= delays[self.preview_idx % delays.len()] as f64
                    && self.preview_frames.len() > 1
                {
                    self.preview_idx = (self.preview_idx + 1) % self.preview_frames.len();
                    self.preview_shown_at = now;
                }
                if self.preview_tex_idx != self.preview_idx {
                    let (w, h, rgba) = &self.preview_frames[self.preview_idx];
                    self.preview_tex = Some(ui.ctx().load_texture(
                        "gif-preview",
                        ColorImage::from_rgba_unmultiplied([*w as usize, *h as usize], rgba),
                        Default::default(),
                    ));
                    self.preview_tex_idx = self.preview_idx;
                }
                if let Some(tex) = &self.preview_tex {
                    ui.separator();
                    ui.add(egui::Image::from_texture(tex).max_width(360.0));
                }
            }
        });

        ui.ctx().request_repaint();
    }
}

/// Parameters for a single capture (+ convert) job.
struct CaptureJob {
    mode: Mode,
    region: Option<Region>,
    fps: u32,
    duration: f32,
    width: Option<u32>,
    optimize: bool,
    output: PathBuf,
    backend_name: Option<String>,
}

/// How the captured image maps to global screen coordinates.
#[derive(Clone, Copy)]
struct ShotInfo {
    /// Global logical origin of the captured output.
    origin_x: i32,
    origin_y: i32,
    /// Logical size of the captured output (per axis).
    out_w: f32,
    out_h: f32,
}

impl Default for ShotInfo {
    fn default() -> Self {
        Self {
            origin_x: 0,
            origin_y: 0,
            out_w: 1.0,
            out_h: 1.0,
        }
    }
}

/// Capture the output the overlay window is on, as raw RGBA pixels.
///
/// Prefers the native Wayland screencopy protocol (wlroots compositors, see
/// [`crate::screenshot`]) and falls back to `grim` on compositors that don't
/// implement it.
fn capture_screen(frame: &eframe::Frame) -> Result<RgbaShot> {
    match native_capture(frame) {
        Ok(shot) => Ok(shot),
        Err(e) => {
            eprintln!("[wlgif] native screencopy failed ({e}); falling back to grim");
            grim_capture()
        }
    }
}

/// A captured frame as flat RGBA pixels plus the output's global geometry.
struct RgbaShot {
    width: usize,
    height: usize,
    rgba: Vec<u8>,
    info: ShotInfo,
}

/// Native capture via `zwlr_screencopy_manager_v1` (wlroots compositors).
fn native_capture(frame: &eframe::Frame) -> Result<RgbaShot> {
    let mut caps = screenshot::Capture::new().map_err(|e| anyhow::anyhow!("{e}"))?;
    let (output, info) = match_monitor(&caps, frame)?;
    let cap = caps
        .capture_output(&output, false)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let rgba = screenshot::to_rgba8(&cap).map_err(|e| anyhow::anyhow!("{e}"))?;
    // `zxdg_output` is implemented by every current compositor; if it is
    // missing we have no logical geometry, so assume an unscaled output at
    // the origin (single-monitor setups — correct; multi-monitor — best
    // effort).
    let (origin_x, origin_y, out_w, out_h) = if info.width == 0 || info.height == 0 {
        (0, 0, cap.width as f32, cap.height as f32)
    } else {
        (info.x, info.y, info.width as f32, info.height as f32)
    };
    Ok(RgbaShot {
        width: cap.width as usize,
        height: cap.height as usize,
        rgba,
        info: ShotInfo {
            origin_x,
            origin_y,
            out_w,
            out_h,
        },
    })
}

/// Pick the `wl_output` that matches the monitor the window is on.
fn match_monitor(
    caps: &screenshot::Capture,
    frame: &eframe::Frame,
) -> Result<(WlOutput, screenshot::OutputInfo)> {
    let monitor = frame
        .winit_window()
        .context("no winit window available")?
        .current_monitor()
        .context("no current monitor")?;
    let mons = caps.outputs();
    if mons.is_empty() {
        anyhow::bail!("compositor reports no outputs");
    }
    if mons.len() == 1 {
        let (o, i) = &mons[0];
        return Ok((o.clone(), i.clone()));
    }

    let mname = monitor.name();
    let msize = monitor.size();
    let mpos = monitor.position();

    // Score each candidate: exact name match wins; physical size and position
    // (logical geometry × scale) break ties.
    let mut best: Option<(f64, &WlOutput, screenshot::OutputInfo)> = None;
    for (o, i) in mons {
        let mut score = 0.0;
        if i.name.as_deref() == mname.as_deref() {
            score += 100.0;
        }
        let phys_w = i.width as f64 * i.scale as f64;
        let phys_h = i.height as f64 * i.scale as f64;
        if (phys_w - msize.width as f64).abs() <= 2.0 && (phys_h - msize.height as f64).abs() <= 2.0
        {
            score += 10.0;
        }
        let ox = i.x as f64 * i.scale as f64;
        let oy = i.y as f64 * i.scale as f64;
        if (ox - mpos.x as f64).abs() <= 2.0 && (oy - mpos.y as f64).abs() <= 2.0 {
            score += 5.0;
        }
        if best.as_ref().is_none_or(|(s, _, _)| score > *s) {
            best = Some((score, o, i.clone()));
        }
    }
    best.map(|(_, o, i)| (o.clone(), i))
        .ok_or_else(|| anyhow::anyhow!("no matching output found"))
}

/// `grim` fallback for compositors without the screencopy protocol (GNOME,
/// KDE, ...).
///
/// We can't identify the focused output without compositor-specific
/// tooling, so `grim` captures the whole span: correct for single-monitor
/// setups (scale 1:1, origin 0,0), best-effort otherwise.
fn grim_capture() -> Result<RgbaShot> {
    let temp = tempfile::NamedTempFile::new().context("failed to create temp file")?;
    let status = Command::new("grim")
        .arg(temp.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("failed to run grim — is it installed?")?;
    if !status.success() {
        anyhow::bail!("grim exited with {status}");
    }
    let bytes = fs::read(temp.path()).context("failed to read screenshot")?;
    let (width, height, rgba) = decode_png_rgba(bytes)?;
    Ok(RgbaShot {
        width,
        height,
        rgba,
        // `grim` gives us no logical geometry: assume an unscaled output at
        // the origin (single-monitor setups — correct).
        info: ShotInfo {
            out_w: width as f32,
            out_h: height as f32,
            ..ShotInfo::default()
        },
    })
}

/// Decode PNG bytes into a flat RGBA buffer: (width, height, pixels).
///
/// egui ships no image decoders, so `Image::from_bytes` fails with "no image
/// loaders are installed". We decode with the `image` crate and hand egui raw
/// pixels via [`ColorImage`], which needs no loader.
fn decode_png_rgba(bytes: Vec<u8>) -> Result<(usize, usize, Vec<u8>)> {
    let img = image::load_from_memory(&bytes).context("failed to decode screenshot PNG")?;
    let rgba = img.to_rgba8();
    Ok((
        rgba.width() as usize,
        rgba.height() as usize,
        rgba.as_raw().to_vec(),
    ))
}

/// Register an installed CJK font as a fallback so Chinese text renders.
///
/// egui's bundled fonts have no CJK glyphs, so without this, Chinese file
/// names and UI text show up as empty boxes. The font is loaded from disk
/// at startup (not embedded) to keep the binary small.
fn setup_fonts(cc: &eframe::CreationContext<'_>) {
    let Some(path) = find_cjk_font() else {
        return;
    };
    let Ok(bytes) = fs::read(&path) else {
        return;
    };

    let mut definitions = FontDefinitions::default();
    let id = "wlgif-cjk".to_owned();
    definitions
        .font_data
        .insert(id.clone(), std::sync::Arc::new(FontData::from_owned(bytes)));
    // Append as a fallback so Latin text keeps using the default fonts.
    if let Some(family) = definitions.families.get_mut(&FontFamily::Proportional) {
        family.push(id.clone());
    }
    if let Some(family) = definitions.families.get_mut(&FontFamily::Monospace) {
        family.push(id);
    }

    cc.egui_ctx.set_fonts(definitions);
}

/// Filename fragments that identify CJK fonts.
const CJK_HINTS: [&str; 12] = [
    "cjk",
    "wqy",
    "zenhei",
    "microhei",
    "sourcehan",
    "droidsansfallback",
    "notosanssc",
    "notosanscjk",
    "uming",
    "ukai",
    "arphic",
    "hanmono",
];

/// Search common font directories for an installed CJK font, preferring
/// the smallest file (e.g. wqy-microhei over a 100MB Noto CJK collection).
fn find_cjk_font() -> Option<PathBuf> {
    let home = std::env::var("HOME").unwrap_or_default();
    let roots = [
        Path::new("/usr/share/fonts"),
        Path::new("/usr/local/share/fonts"),
        &Path::new(&home).join(".fonts"),
        &Path::new(&home).join(".local/share/fonts"),
    ];

    let mut candidates = Vec::new();
    for root in roots {
        collect_cjk_fonts(&mut candidates, root, 0);
    }
    candidates.sort_by_key(|p| fs::metadata(p).map(|m| m.len()).unwrap_or(u64::MAX));
    candidates.into_iter().next()
}

fn collect_cjk_fonts(candidates: &mut Vec<PathBuf>, dir: &Path, depth: u8) {
    if depth > 4 {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let entries: Vec<_> = entries.flatten().collect();

    for entry in &entries {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let lower = name.to_lowercase();
        let is_font = ["ttf", "ttc", "otf"].iter().any(|ext| lower.ends_with(ext));
        if is_font && CJK_HINTS.iter().any(|hint| lower.contains(hint)) {
            candidates.push(path);
        }
    }
    for entry in &entries {
        let path = entry.path();
        if path.is_dir() {
            collect_cjk_fonts(candidates, &path, depth + 1);
        }
    }
}

/// Run a capture job in a background thread.
fn record_and_convert(
    params: &CaptureJob,
    stop: &Arc<AtomicBool>,
    job: &Arc<Mutex<JobState>>,
) -> Result<()> {
    // Screenshots are a single frame: no recording backend involved.
    if params.mode == Mode::Shot {
        return capture_and_save(params);
    }

    let backend = match &params.backend_name {
        Some(name) => backend::by_name(name)?,
        None => backend::detect()?,
    };
    if let Err(err) = backend.is_available() {
        anyhow::bail!("后端 '{}' 不可用: {}", backend.name(), err);
    }

    let temp = tempfile::TempDir::new().context("failed to create temp directory")?;
    let video = temp.path().join("capture.mp4");

    let config = RecordConfig {
        fps: params.fps,
        duration: params.duration,
        quiet: true,
        stop: Some(Arc::clone(stop)),
    };
    backend.record(params.region.as_ref(), &video, &config)?;

    if fs::metadata(&video).map(|m| m.len()).unwrap_or(0) == 0 {
        return Err(Error::EmptyRecording.into());
    }

    if params.mode == Mode::Video {
        // No conversion: move the captured video to the output path (only on
        // success, so a failed run doesn't clobber the last good file).
        if fs::rename(&video, &params.output).is_err() {
            fs::copy(&video, &params.output).context("failed to save video")?;
        }
        return Ok(());
    }

    {
        let mut j = job.lock().unwrap();
        j.phase = Phase::Converting;
    }
    converter::to_gif(
        &video,
        &params.output,
        params.fps,
        params.width,
        params.optimize,
        true,
        Some(&mut |p| {
            let mut j = job.lock().unwrap();
            j.progress = p;
        }),
    )?;
    Ok(())
}

/// Capture the region (or the whole first output) as a single frame and
/// save it as a PNG. Uses the native screencopy protocol directly — no
/// recording backend and no ffmpeg involved.
fn capture_and_save(params: &CaptureJob) -> Result<()> {
    let mut caps = screenshot::Capture::new()
        .map_err(|e| anyhow::anyhow!("failed to connect to compositor: {e}"))?;
    let outputs = caps.outputs();
    if outputs.is_empty() {
        anyhow::bail!("compositor reports no outputs");
    }

    // Output-local logical coordinates of the capture region.
    let (output, (x, y, w, h)) = match &params.region {
        Some(r) => {
            let (rx, ry) = (r.x as i32, r.y as i32);
            let (o, info) = outputs
                .iter()
                .find(|(_, i)| {
                    let (ox, oy) = (i.x, i.y);
                    let (ow, oh) = (i.width as i32, i.height as i32);
                    rx >= ox && ry >= oy && rx < ox + ow && ry < oy + oh
                })
                .cloned()
                .ok_or_else(|| {
                    anyhow::anyhow!("region ({}, {}) is outside every known output", r.x, r.y)
                })?;
            let (ox, oy) = (info.x, info.y);
            let (ow, oh) = (info.width as i32, info.height as i32);
            let x = rx - ox;
            let y = ry - oy;
            let w = (r.width as i32).min(ow - x).max(1);
            let h = (r.height as i32).min(oh - y).max(1);
            (o, (x, y, w, h))
        }
        None => {
            let (o, info) = &outputs[0];
            (o.clone(), (0, 0, info.width as i32, info.height as i32))
        }
    };

    let cap = caps
        .capture_output_region(&output, x, y, w, h, true)
        .map_err(|e| anyhow::anyhow!("screenshot failed: {e}"))?;
    let rgba = screenshot::to_rgba8(&cap).map_err(|e| anyhow::anyhow!("{e}"))?;
    let img = image::RgbaImage::from_raw(cap.width, cap.height, rgba)
        .context("failed to build image")?;
    img.save(&params.output)
        .with_context(|| format!("failed to write {}", params.output.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a small 3-frame animated GIF in memory (red/green/blue).
    fn make_gif() -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut enc = image::codecs::gif::GifEncoder::new(&mut buf);
            enc.set_repeat(image::codecs::gif::Repeat::Infinite).unwrap();
            for (i, c) in [[255u8, 0, 0], [0, 255, 0], [0, 0, 255]].into_iter().enumerate() {
                let img =
                    image::RgbaImage::from_pixel(4, 3, image::Rgba([c[0], c[1], c[2], 255]));
                let delay = image::Delay::from_numer_denom_ms(50 + i as u32 * 10, 1);
                enc.encode_frame(image::Frame::from_parts(img, 0, 0, delay))
                    .unwrap();
            }
        }
        buf
    }

    #[test]
    fn decode_gif_preview_decodes_frames_and_delays() {
        let (frames, delays) = WlgifApp::decode_gif_preview(&make_gif()).unwrap();
        assert_eq!(frames.len(), 3);
        assert_eq!((frames[0].0, frames[0].1), (4, 3));
        assert_eq!(frames[0].2.len(), 4 * 3 * 4);
        // Frame 0 is opaque red.
        assert_eq!(&frames[0].2[0..4], [255, 0, 0, 255]);
        // Frame 2 is opaque blue.
        assert_eq!(&frames[2].2[0..4], [0, 0, 255, 255]);
        // Delays: 50 ms, 60 ms, 70 ms.
        assert!((delays[0] - 0.05).abs() < 1e-6);
        assert!((delays[2] - 0.07).abs() < 1e-6);
    }

    #[test]
    fn decode_gif_preview_rejects_non_gif() {
        assert!(WlgifApp::decode_gif_preview(b"definitely not a gif").is_none());
    }

    #[test]
    fn decode_png_preview_decodes_single_frame() {
        use image::ImageEncoder;
        let img = image::RgbaImage::from_pixel(4, 3, image::Rgba([10, 20, 30, 255]));
        let mut buf = Vec::new();
        image::codecs::png::PngEncoder::new(&mut buf)
            .write_image(img.as_raw(), 4, 3, image::ColorType::Rgba8.into())
            .unwrap();
        let (frames, _delays) = WlgifApp::decode_png_preview(&buf).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!((frames[0].0, frames[0].1), (4, 3));
        assert_eq!(&frames[0].2[0..4], [10, 20, 30, 255]);
    }
}
