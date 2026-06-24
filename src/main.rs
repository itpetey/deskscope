use std::{
    marker::PhantomData,
    num::NonZeroU32,
    path::PathBuf,
    sync::{mpsc, Arc, Mutex},
    time::{Duration, Instant},
};

use chrono::Local;
use clap::Parser;
use drm_fourcc::DrmFourcc;
use libcamera::{
    camera::CameraConfigurationStatus,
    camera_manager::CameraManager,
    framebuffer_allocator::{FrameBuffer, FrameBufferAllocator},
    framebuffer_map::MemoryMappedFrameBuffer,
    geometry::Size,
    pixel_format::PixelFormat,
    properties,
    request::ReuseFlag,
    stream::StreamRole,
};
use serde::Deserialize;
use winit::{
    application::ApplicationHandler,
    dpi::PhysicalPosition,
    event::{ElementState, MouseButton, TouchPhase, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop},
    window::{Fullscreen, Window, WindowId},
};

const PIXEL_FORMAT_RGB888: PixelFormat = PixelFormat::new(DrmFourcc::Rgb888 as u32, 0);
const MENU_BG: u32 = 0xCC00_0000;
const BTN_POWER_OFF: u32 = 0xFFCC_3333;
const BTN_TAKE_PHOTO: u32 = 0xFF33_CC33;
const BTN_SETTING_ORIENT: u32 = 0xFF44_4488;
const BTN_SETTING_FLIP_V: u32 = 0xFF44_4488;
const BTN_SETTING_FLIP_H: u32 = 0xFF44_4488;
const BTN_BORDER: u32 = 0xFFFFFFFF;
const TEXT_COLOR: u32 = 0xFFFFFFFF;
const TEXT_HIGHLIGHT: u32 = 0xFF33_CCFF;
const STATUS_COLOR: u32 = 0xFF33_CCFF;

#[derive(Debug, Clone, Copy, PartialEq)]
enum Orientation {
    Landscape,
    Portrait,
}

#[derive(Debug, Clone)]
struct Settings {
    orientation: Orientation,
    flip_vertical: bool,
    flip_horizontal: bool,
}

/// CLI arguments for deskscope.
#[derive(Debug, Parser)]
#[command(version, about = "GUI for interacting with a Raspberry Pi camera")]
struct Args {
    /// Path to the TOML configuration file
    #[arg(short, long, default_value = "/usr/local/etc/deskscope.toml")]
    config: PathBuf,
}

#[derive(Debug, Deserialize)]
struct Config {
    photo_directory: PathBuf,

    /// Number of seconds of inactivity before auto power-off.
    /// Set to 0 or omit to disable auto power-off.
    #[serde(default)]
    power_off_after_secs: u64,

    /// Image and menu orientation: "landscape" or "portrait".
    /// Defaults to "landscape".
    #[serde(default = "default_orientation")]
    orientation: String,

    /// Whether to vertically flip the camera image.
    /// Defaults to false.
    #[serde(default)]
    flip_vertical: bool,

    /// Whether to horizontally flip the camera image.
    /// Defaults to false.
    #[serde(default)]
    flip_horizontal: bool,
}

fn default_orientation() -> String {
    "landscape".to_string()
}

fn load_config() -> Config {
    let args = Args::parse();

    if !args.config.exists() {
        panic!(
            "Config file not found: {}\n  Create it or omit --config to use defaults.",
            args.config.display()
        );
    }

    let contents = std::fs::read_to_string(&args.config)
        .unwrap_or_else(|e| panic!("Failed to read config file {}: {e}", args.config.display()));
    toml::from_str(&contents)
        .unwrap_or_else(|e| panic!("Failed to parse config file {}: {e}", args.config.display()))
}

struct Button {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    label: String,
    color: u32,
}

struct App<'a> {
    _mgr: PhantomData<&'a CameraManager>,
    cam: libcamera::camera::ActiveCamera<'a>,
    latest_frame: Arc<Mutex<Vec<u8>>>,
    req_rx: mpsc::Receiver<libcamera::request::Request>,
    actual_size: Size,
    window_size: Option<winit::dpi::PhysicalSize<u32>>,
    window: Option<Arc<Window>>,
    context: Option<softbuffer::Context<Arc<Window>>>,
    surface: Option<softbuffer::Surface<Arc<Window>, Arc<Window>>>,
    config: Config,
    settings: Settings,
    menu_visible: bool,
    powered_off: bool,
    last_activity: Instant,
    last_pointer_pos: Option<PhysicalPosition<f64>>,
    status_msg: Option<String>,
    status_until: Option<Instant>,

    /// Receiver channel for GPIO interrupt notifications (pin 21 = power toggle).
    gpio_rx: mpsc::Receiver<()>,
}

impl<'a> App<'a> {
    fn buttons(&self, win_w: f64, win_h: f64) -> Vec<Button> {
        let btn_w_main = win_w * 0.80;
        let btn_w_toggle = match self.settings.orientation {
            Orientation::Landscape => win_w * 0.80,
            Orientation::Portrait => win_w * 0.60,
        };
        let count = 5u32;
        let btn_h = win_h * 0.12;
        let gap = win_h * 0.025;
        let total_h = count as f64 * btn_h + (count - 1) as f64 * gap;
        let start_y = (win_h - total_h) / 2.0;
        let x_main = (win_w - btn_w_main) / 2.0;
        let x_toggle = (win_w - btn_w_toggle) / 2.0;

        let orient_label = format!(
            "Orientation: {}",
            match self.settings.orientation {
                Orientation::Landscape => "Landscape",
                Orientation::Portrait => "Portrait",
            }
        );
        let flip_v_label = format!(
            "Flip V: {}",
            if self.settings.flip_vertical {
                "On"
            } else {
                "Off"
            }
        );
        let flip_h_label = format!(
            "Flip H: {}",
            if self.settings.flip_horizontal {
                "On"
            } else {
                "Off"
            }
        );

        let mut y = start_y;
        let mut items = Vec::new();

        // 0: Power off
        items.push(Button {
            x: x_main,
            y,
            w: btn_w_main,
            h: btn_h,
            label: "Power off".to_string(),
            color: BTN_POWER_OFF,
        });
        y += btn_h + gap;

        // 1: Take photo
        items.push(Button {
            x: x_main,
            y,
            w: btn_w_main,
            h: btn_h,
            label: "Take photo".to_string(),
            color: BTN_TAKE_PHOTO,
        });
        y += btn_h + gap;

        // 2: Orientation toggle (camera + menu)
        items.push(Button {
            x: x_toggle,
            y,
            w: btn_w_toggle,
            h: btn_h,
            label: orient_label,
            color: BTN_SETTING_ORIENT,
        });
        y += btn_h + gap;

        // 3: Flip vertical toggle
        items.push(Button {
            x: x_toggle,
            y,
            w: btn_w_toggle,
            h: btn_h,
            label: flip_v_label,
            color: BTN_SETTING_FLIP_V,
        });
        y += btn_h + gap;

        // 4: Flip horizontal toggle
        items.push(Button {
            x: x_toggle,
            y,
            w: btn_w_toggle,
            h: btn_h,
            label: flip_h_label,
            color: BTN_SETTING_FLIP_H,
        });

        items
    }

    fn handle_pointer(&mut self, x: f64, y: f64) {
        self.last_activity = Instant::now();

        if self.powered_off {
            // Any tap while powered off wakes the device (starts video) without showing the menu.
            self.powered_off = false;
            return;
        }

        let Some(size) = self.window_size else { return };
        let win_w = size.width as f64;
        let win_h = size.height as f64;

        if !self.menu_visible {
            self.menu_visible = true;
            return;
        }

        for (i, btn) in self.buttons(win_w, win_h).iter().enumerate() {
            if x >= btn.x && x < btn.x + btn.w && y >= btn.y && y < btn.y + btn.h {
                match i {
                    0 => self.powered_off = true,
                    1 => self.save_photo(),
                    2 => {
                        self.settings.orientation = match self.settings.orientation {
                            Orientation::Landscape => Orientation::Portrait,
                            Orientation::Portrait => Orientation::Landscape,
                        };
                    }
                    3 => {
                        self.settings.flip_vertical = !self.settings.flip_vertical;
                    }
                    4 => {
                        self.settings.flip_horizontal = !self.settings.flip_horizontal;
                    }
                    _ => {}
                }
                // Don't dismiss menu on toggle taps so user can see the change.
                if i >= 2 {
                    return;
                }
                self.menu_visible = false;
                return;
            }
        }

        // Tap outside the buttons dismisses the menu.
        self.menu_visible = false;
    }

    fn save_photo(&mut self) {
        let width = self.actual_size.width;
        let height = self.actual_size.height;
        let expected = (width as usize) * (height as usize) * 3;

        let frame_data = {
            let frame = self.latest_frame.lock().unwrap();
            if frame.is_empty() {
                drop(frame);
                self.show_status("No frame to save".to_string());
                return;
            }
            if frame.len() < expected {
                let len = frame.len();
                drop(frame);
                self.show_status(format!("Frame buffer too small: {len} < {expected}"));
                return;
            }
            frame[..expected].to_vec()
        };

        let img = match image::RgbImage::from_raw(width, height, frame_data) {
            Some(img) => img,
            None => {
                self.show_status("Failed to build image".to_string());
                return;
            }
        };

        let dir = &self.config.photo_directory;
        if let Err(e) = std::fs::create_dir_all(dir) {
            self.show_status(format!("Cannot create photo dir: {e}"));
            return;
        }

        let filename = format!("deskscope_{}.png", Local::now().format("%Y%m%d_%H%M%S"));
        let path = dir.join(&filename);

        match img.save(&path) {
            Ok(_) => {
                println!("Saved photo to {}", path.display());
                self.show_status(format!("Saved {filename}"));
            }
            Err(e) => {
                eprintln!("Failed to save photo: {e}");
                self.show_status(format!("Save failed: {e}"));
            }
        }
    }

    fn toggle_power(&mut self) {
        self.last_activity = Instant::now();
        self.powered_off = !self.powered_off;
        self.menu_visible = false;
    }

    fn show_status(&mut self, msg: String) {
        self.status_msg = Some(msg);
        self.status_until = Some(Instant::now() + Duration::from_secs(2));
    }
}

impl<'a> ApplicationHandler for App<'a> {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }

        event_loop.set_control_flow(ControlFlow::Poll);

        let window = Arc::new(
            event_loop
                .create_window(
                    Window::default_attributes()
                        .with_title("Deskscope")
                        .with_fullscreen(Some(Fullscreen::Borderless(None))),
                )
                .unwrap(),
        );

        let context = softbuffer::Context::new(Arc::clone(&window)).unwrap();
        let mut surface = softbuffer::Surface::new(&context, Arc::clone(&window)).unwrap();

        let window_size = window.inner_size();
        surface
            .resize(
                NonZeroU32::new(window_size.width).unwrap(),
                NonZeroU32::new(window_size.height).unwrap(),
            )
            .unwrap();

        println!(
            "Camera stream: {}x{} — Window: {}x{}",
            self.actual_size.width, self.actual_size.height, window_size.width, window_size.height
        );

        self.window = Some(window);
        self.context = Some(context);
        self.surface = Some(surface);
        self.window_size = Some(window_size);
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => {
                println!("Shutting down");
                event_loop.exit();
            }
            WindowEvent::Resized(new_size) => {
                if let Some(surface) = self.surface.as_mut() {
                    surface
                        .resize(
                            NonZeroU32::new(new_size.width).unwrap(),
                            NonZeroU32::new(new_size.height).unwrap(),
                        )
                        .ok();
                    self.window_size = Some(new_size);
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.last_pointer_pos = Some(position);
            }
            WindowEvent::Touch(touch) => {
                if touch.phase == TouchPhase::Started {
                    let pos = touch.location;
                    self.handle_pointer(pos.x, pos.y);
                }
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                ..
            } => {
                if let Some(pos) = self.last_pointer_pos {
                    self.handle_pointer(pos.x, pos.y);
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        // Re-queue completed requests so the camera keeps streaming.
        while let Ok(mut req) = self.req_rx.try_recv() {
            req.reuse(ReuseFlag::REUSE_BUFFERS);
            if let Err(e) = self.cam.queue_request(req) {
                eprintln!("Failed to re-queue request: {e:?}");
            }
        }

        // Auto power-off if idle timeout is configured and has elapsed.
        if !self.powered_off {
            let timeout = Duration::from_secs(self.config.power_off_after_secs);
            if timeout > Duration::ZERO && self.last_activity.elapsed() >= timeout {
                self.powered_off = true;
            }
        }

        // Check for GPIO interrupt (pin 21 shorted to ground = power toggle).
        while self.gpio_rx.try_recv().is_ok() {
            self.toggle_power();
        }

        let Some(window_size) = self.window_size else {
            return;
        };
        let buttons = if self.menu_visible {
            Some(self.buttons(window_size.width as f64, window_size.height as f64))
        } else {
            None
        };

        let Some(surface) = self.surface.as_mut() else {
            return;
        };

        let Ok(mut buffer) = surface.buffer_mut() else {
            eprintln!("Failed to acquire softbuffer surface buffer");
            return;
        };

        let win_w = window_size.width as usize;
        let win_h = window_size.height as usize;

        if self.powered_off {
            buffer.fill(0xFF00_0000);
            if let Err(e) = buffer.present() {
                eprintln!("Failed to present frame: {e:?}");
            }
            return;
        }

        // Start with black to cover any area not filled by the camera frame.
        buffer.fill(0xFF00_0000);

        let frame = self.latest_frame.lock().unwrap();
        if !frame.is_empty() {
            let cam_w = self.actual_size.width as usize;
            let cam_h = self.actual_size.height as usize;

            match self.settings.orientation {
                Orientation::Landscape => {
                    let copy_w = cam_w.min(win_w);
                    let copy_h = cam_h.min(win_h);
                    let off_x = (cam_w - copy_w) / 2;
                    let off_y = (cam_h - copy_h) / 2;

                    for y in 0..copy_h {
                        for x in 0..copy_w {
                            let src_x = if self.settings.flip_horizontal {
                                off_x + (copy_w - 1 - x)
                            } else {
                                off_x + x
                            };
                            let src_y = if self.settings.flip_vertical {
                                off_y + (copy_h - 1 - y)
                            } else {
                                off_y + y
                            };
                            let src = (src_y * cam_w + src_x) * 3;
                            let dst = y * win_w + x;
                            let r = frame[src];
                            let g = frame[src + 1];
                            let b = frame[src + 2];
                            // BGRA (little-endian) — softbuffer native format.
                            buffer[dst] = (255u32 << 24)
                                | ((r as u32) << 16)
                                | ((g as u32) << 8)
                                | (b as u32);
                        }
                    }
                }
                Orientation::Portrait => {
                    // 90° clockwise rotation, then apply screen-space flips.
                    // Center-crop the rotated image to keep the same subject visible.
                    let out_w = cam_h;
                    let out_h = cam_w;
                    let copy_w = out_w.min(win_w);
                    let copy_h = out_h.min(win_h);
                    let off_x = (out_w - copy_w) / 2;
                    let off_y = (out_h - copy_h) / 2;

                    for y in 0..copy_h {
                        for x in 0..copy_w {
                            // Position within the full rotated image space.
                            let (src_x, src_y) =
                                if self.settings.flip_horizontal && self.settings.flip_vertical {
                                    // Both flips after 90° CW
                                    (cam_w - 1 - off_y - y, off_x + x)
                                } else if self.settings.flip_horizontal {
                                    // Horizontal flip only after 90° CW
                                    (off_y + y, off_x + x)
                                } else if self.settings.flip_vertical {
                                    // Vertical flip only after 90° CW
                                    (cam_w - 1 - off_y - y, cam_h - 1 - off_x - x)
                                } else {
                                    // Base 90° CW rotation
                                    (off_y + y, cam_h - 1 - off_x - x)
                                };
                            let src = (src_y * cam_w + src_x) * 3;
                            let dst = y * win_w + x;
                            let r = frame[src];
                            let g = frame[src + 1];
                            let b = frame[src + 2];
                            // BGRA (little-endian) — softbuffer native format.
                            buffer[dst] = (255u32 << 24)
                                | ((r as u32) << 16)
                                | ((g as u32) << 8)
                                | (b as u32);
                        }
                    }
                }
            }
        }
        drop(frame);

        let win_wf = window_size.width as f64;
        let win_hf = window_size.height as f64;

        if let Some(buttons) = buttons {
            draw_rect(&mut buffer, win_w, win_h, 0.0, 0.0, win_wf, win_hf, MENU_BG);
            for (i, btn) in buttons.iter().enumerate() {
                draw_rect(
                    &mut buffer,
                    win_w,
                    win_h,
                    btn.x,
                    btn.y,
                    btn.w,
                    btn.h,
                    btn.color,
                );
                draw_rect_border(
                    &mut buffer,
                    win_w,
                    win_h,
                    btn.x,
                    btn.y,
                    btn.w,
                    btn.h,
                    BTN_BORDER,
                    4,
                );

                // Center label in the button.
                let scale = ((btn.w / 10.0).min(btn.h / 5.0) as u32).max(3);
                let text_w = btn.label.chars().count() as u32 * (5 + 1) * scale;
                let text_h = 7 * scale;
                let tx = btn.x + (btn.w - text_w as f64) / 2.0;
                let ty = btn.y + (btn.h - text_h as f64) / 2.0;

                // For toggle items (indices 2-4), draw the value part in highlight color.
                if i >= 2 {
                    // Find the colon separator to split label and value.
                    if let Some(col) = btn.label.find(':') {
                        let left = &btn.label[..=col]; // e.g. "Camera:"
                        let right = &btn.label[col + 2..]; // e.g. "Portrait"
                        let left_w = left.chars().count() as u32 * (5 + 1) * scale;
                        draw_text(
                            &mut buffer,
                            win_w,
                            win_h,
                            left,
                            tx as i32,
                            ty as i32,
                            scale,
                            TEXT_COLOR,
                        );
                        draw_text(
                            &mut buffer,
                            win_w,
                            win_h,
                            right,
                            (tx + left_w as f64) as i32,
                            ty as i32,
                            scale,
                            TEXT_HIGHLIGHT,
                        );
                    } else {
                        draw_text(
                            &mut buffer,
                            win_w,
                            win_h,
                            &btn.label,
                            tx as i32,
                            ty as i32,
                            scale,
                            TEXT_COLOR,
                        );
                    }
                } else {
                    draw_text(
                        &mut buffer,
                        win_w,
                        win_h,
                        &btn.label,
                        tx as i32,
                        ty as i32,
                        scale,
                        TEXT_COLOR,
                    );
                }
            }
        }

        if let Some(until) = self.status_until {
            if Instant::now() > until {
                self.status_msg = None;
                self.status_until = None;
            }
        }
        if let Some(msg) = &self.status_msg {
            let scale = ((win_wf / 20.0).min(win_hf / 10.0) as u32).max(3);
            let text_w = msg.chars().count() as u32 * (5 + 1) * scale;
            let tx = ((win_w as u32).saturating_sub(text_w)) as i32 / 2;
            let ty = (win_h as f64 * 0.1) as i32;
            draw_text(&mut buffer, win_w, win_h, msg, tx, ty, scale, STATUS_COLOR);
        }

        if let Err(e) = buffer.present() {
            eprintln!("Failed to present frame: {e:?}");
        }
    }
}

fn draw_rect(
    buffer: &mut [u32],
    win_w: usize,
    win_h: usize,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    color: u32,
) {
    let x0 = x.clamp(0.0, win_w as f64) as usize;
    let y0 = y.clamp(0.0, win_h as f64) as usize;
    let x1 = ((x + w).clamp(0.0, win_w as f64)) as usize;
    let y1 = ((y + h).clamp(0.0, win_h as f64)) as usize;

    for yp in y0..y1 {
        let row = yp * win_w;
        for xp in x0..x1 {
            buffer[row + xp] = blend(buffer[row + xp], color);
        }
    }
}

fn draw_rect_border(
    buffer: &mut [u32],
    win_w: usize,
    win_h: usize,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    color: u32,
    thickness: u32,
) {
    let t = thickness as f64;
    // Top and bottom borders.
    draw_rect(buffer, win_w, win_h, x, y, w, t, color);
    draw_rect(buffer, win_w, win_h, x, y + h - t, w, t, color);
    // Left and right borders.
    draw_rect(buffer, win_w, win_h, x, y, t, h, color);
    draw_rect(buffer, win_w, win_h, x + w - t, y, t, h, color);
}

fn blend(dst: u32, src: u32) -> u32 {
    let sa = ((src >> 24) & 0xFF) as u32;
    if sa == 255 {
        return src;
    }
    let da = ((dst >> 24) & 0xFF) as u32;
    let inv_sa = 255 - sa;
    let a = sa + (da * inv_sa) / 255;
    let r = ((src >> 16) & 0xFF) * sa + ((dst >> 16) & 0xFF) * inv_sa;
    let g = ((src >> 8) & 0xFF) * sa + ((dst >> 8) & 0xFF) * inv_sa;
    let b = (src & 0xFF) * sa + (dst & 0xFF) * inv_sa;
    (a << 24) | ((r / 255) << 16) | ((g / 255) << 8) | (b / 255)
}

fn draw_text(
    buffer: &mut [u32],
    win_w: usize,
    win_h: usize,
    text: &str,
    x: i32,
    y: i32,
    scale: u32,
    color: u32,
) {
    let mut cx = x;
    for ch in text.chars() {
        draw_char(buffer, win_w, win_h, ch, cx, y, scale, color);
        cx += (5 + 1) as i32 * scale as i32;
    }
}

fn draw_char(
    buffer: &mut [u32],
    win_w: usize,
    win_h: usize,
    ch: char,
    x: i32,
    y: i32,
    scale: u32,
    color: u32,
) {
    let rows = match font_bitmap(ch) {
        Some(rows) => rows,
        None => return,
    };

    for (row, byte) in rows.iter().enumerate() {
        for col in 0..5 {
            let bit = 1 << (4 - col);
            if byte & bit != 0 {
                let px = x + col as i32 * scale as i32;
                let py = y + row as i32 * scale as i32;
                draw_rect(
                    buffer,
                    win_w,
                    win_h,
                    px as f64,
                    py as f64,
                    scale as f64,
                    scale as f64,
                    color,
                );
            }
        }
    }
}

fn font_bitmap(ch: char) -> Option<[u8; 7]> {
    Some(match ch {
        'A' => [0x0E, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11],
        'B' => [0x1E, 0x11, 0x1E, 0x11, 0x11, 0x11, 0x1E],
        'C' => [0x0E, 0x11, 0x10, 0x10, 0x10, 0x11, 0x0E],
        'D' => [0x1E, 0x11, 0x11, 0x11, 0x11, 0x11, 0x1E],
        'E' => [0x1F, 0x10, 0x10, 0x1E, 0x10, 0x10, 0x1F],
        'F' => [0x1F, 0x10, 0x10, 0x1E, 0x10, 0x10, 0x10],
        'G' => [0x0E, 0x11, 0x10, 0x17, 0x11, 0x11, 0x0E],
        'H' => [0x11, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11],
        'I' => [0x0E, 0x04, 0x04, 0x04, 0x04, 0x04, 0x0E],
        'J' => [0x01, 0x01, 0x01, 0x01, 0x11, 0x11, 0x0E],
        'K' => [0x11, 0x12, 0x14, 0x18, 0x14, 0x12, 0x11],
        'L' => [0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x1F],
        'M' => [0x11, 0x1B, 0x15, 0x11, 0x11, 0x11, 0x11],
        'N' => [0x11, 0x19, 0x15, 0x13, 0x11, 0x11, 0x11],
        'O' => [0x0E, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E],
        'P' => [0x0E, 0x09, 0x0E, 0x08, 0x08, 0x08, 0x08],
        'Q' => [0x0E, 0x11, 0x11, 0x11, 0x15, 0x12, 0x0D],
        'R' => [0x0E, 0x09, 0x0E, 0x0A, 0x09, 0x09, 0x09],
        'S' => [0x00, 0x0E, 0x08, 0x0E, 0x02, 0x0E, 0x00],
        'T' => [0x1F, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04],
        'U' => [0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E],
        'V' => [0x11, 0x11, 0x11, 0x11, 0x11, 0x0A, 0x04],
        'W' => [0x11, 0x11, 0x11, 0x15, 0x15, 0x15, 0x0A],
        'X' => [0x11, 0x11, 0x0A, 0x04, 0x0A, 0x11, 0x11],
        'Y' => [0x11, 0x11, 0x0A, 0x04, 0x04, 0x04, 0x04],
        'Z' => [0x1F, 0x01, 0x02, 0x04, 0x08, 0x10, 0x1F],
        'a' => [0x00, 0x00, 0x0E, 0x02, 0x0E, 0x0A, 0x0E],
        'b' => [0x08, 0x08, 0x0E, 0x09, 0x09, 0x09, 0x0E],
        'c' => [0x00, 0x00, 0x0E, 0x08, 0x08, 0x08, 0x0E],
        'd' => [0x02, 0x02, 0x0E, 0x0A, 0x0A, 0x0A, 0x0E],
        'e' => [0x00, 0x00, 0x0E, 0x0A, 0x1E, 0x08, 0x0E],
        'f' => [0x00, 0x06, 0x08, 0x0E, 0x08, 0x08, 0x08],
        'g' => [0x00, 0x00, 0x0E, 0x0A, 0x0E, 0x02, 0x0E],
        'h' => [0x08, 0x08, 0x0C, 0x0A, 0x0A, 0x0A, 0x0A],
        'i' => [0x00, 0x04, 0x00, 0x04, 0x04, 0x04, 0x04],
        'j' => [0x00, 0x02, 0x00, 0x02, 0x02, 0x12, 0x0C],
        'k' => [0x08, 0x08, 0x0A, 0x0C, 0x0A, 0x09, 0x09],
        'l' => [0x0C, 0x04, 0x04, 0x04, 0x04, 0x04, 0x06],
        'm' => [0x00, 0x00, 0x16, 0x1B, 0x1B, 0x11, 0x11],
        'n' => [0x00, 0x00, 0x0E, 0x09, 0x09, 0x09, 0x09],
        'o' => [0x00, 0x00, 0x0E, 0x09, 0x09, 0x09, 0x0E],
        'p' => [0x00, 0x00, 0x0E, 0x09, 0x0E, 0x08, 0x08],
        'q' => [0x00, 0x00, 0x0E, 0x09, 0x0E, 0x02, 0x02],
        'r' => [0x00, 0x00, 0x0C, 0x0A, 0x08, 0x08, 0x08],
        's' => [0x00, 0x00, 0x0E, 0x08, 0x0E, 0x02, 0x0E],
        't' => [0x00, 0x08, 0x0E, 0x08, 0x08, 0x09, 0x06],
        'u' => [0x00, 0x00, 0x09, 0x09, 0x09, 0x09, 0x0E],
        'v' => [0x00, 0x00, 0x09, 0x09, 0x09, 0x05, 0x02],
        'w' => [0x00, 0x00, 0x11, 0x11, 0x15, 0x15, 0x0A],
        'x' => [0x00, 0x00, 0x09, 0x06, 0x04, 0x06, 0x09],
        'y' => [0x00, 0x00, 0x09, 0x09, 0x0E, 0x02, 0x0E],
        'z' => [0x00, 0x00, 0x1F, 0x02, 0x04, 0x08, 0x1F],
        '0' => [0x0E, 0x11, 0x13, 0x15, 0x19, 0x11, 0x0E],
        '1' => [0x04, 0x0C, 0x04, 0x04, 0x04, 0x04, 0x0E],
        '2' => [0x0E, 0x11, 0x01, 0x02, 0x04, 0x08, 0x1F],
        '3' => [0x1F, 0x02, 0x04, 0x0E, 0x01, 0x11, 0x0E],
        '4' => [0x02, 0x06, 0x0A, 0x12, 0x1F, 0x02, 0x02],
        '5' => [0x1F, 0x10, 0x1E, 0x01, 0x01, 0x11, 0x0E],
        '6' => [0x0E, 0x11, 0x10, 0x1E, 0x11, 0x11, 0x0E],
        '7' => [0x1F, 0x01, 0x02, 0x04, 0x08, 0x08, 0x08],
        '8' => [0x0E, 0x11, 0x11, 0x0E, 0x11, 0x11, 0x0E],
        '9' => [0x0E, 0x11, 0x11, 0x0F, 0x01, 0x11, 0x0E],
        '!' => [0x00, 0x04, 0x04, 0x04, 0x00, 0x04, 0x00],
        ':' => [0x00, 0x00, 0x04, 0x00, 0x04, 0x00, 0x00],
        '-' => [0x00, 0x00, 0x00, 0x1F, 0x00, 0x00, 0x00],
        '_' => [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x1F],
        '.' => [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x04],
        ',' => [0x00, 0x00, 0x00, 0x00, 0x00, 0x04, 0x08],
        '<' => [0x00, 0x02, 0x04, 0x08, 0x04, 0x02, 0x00],
        '>' => [0x00, 0x08, 0x04, 0x02, 0x04, 0x08, 0x00],
        '/' => [0x00, 0x01, 0x02, 0x04, 0x08, 0x10, 0x00],
        ' ' => [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
        _ => return None,
    })
}

fn main() {
    let config = load_config();
    println!("Photo directory: {}", config.photo_directory.display());

    let size = Size {
        width: 1280,
        height: 720,
    };

    let mgr = CameraManager::new().unwrap();
    let cameras = mgr.cameras();
    let cam = cameras.get(0).expect("no cameras found");
    println!(
        "Using camera: {}",
        *cam.properties().get::<properties::Model>().unwrap()
    );

    let mut cam = cam.acquire().expect("unable to acquire camera");

    let mut cfgs = cam
        .generate_configuration(&[StreamRole::ViewFinder])
        .unwrap();

    cfgs.get_mut(0)
        .unwrap()
        .set_pixel_format(PIXEL_FORMAT_RGB888);
    cfgs.get_mut(0).unwrap().set_size(size);

    match cfgs.validate() {
        CameraConfigurationStatus::Valid => println!("Camera configuration valid"),
        CameraConfigurationStatus::Adjusted => {
            println!("Camera configuration adjusted: {:#?}", cfgs)
        }
        CameraConfigurationStatus::Invalid => panic!("invalid camera configuration"),
    }

    let actual_size = cfgs.get(0).unwrap().get_size();

    cam.configure(&mut cfgs)
        .expect("unable to configure camera");

    // Allocate and map frame buffers.
    let mut alloc = FrameBufferAllocator::new(&cam);
    let stream = cfgs
        .get(0)
        .expect("missing stream config")
        .stream()
        .expect("missing stream");
    let buffers = alloc.alloc(&stream).expect("failed to allocate buffers");
    println!("Allocated {} buffers", buffers.len());

    let buffers: Vec<_> = buffers
        .into_iter()
        .map(|buf| MemoryMappedFrameBuffer::new(buf).expect("failed to map buffer"))
        .collect();

    // Shared latest frame between camera callback and rendering thread.
    let latest_frame: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));

    // Channel to return completed requests for re-queueing.
    let (req_tx, req_rx) = mpsc::channel();

    let stream_clone = stream.clone();
    let latest_frame_clone = Arc::clone(&latest_frame);

    // Register callback: fires on libcamera's internal thread when a request completes.
    cam.on_request_completed(move |req| {
        // Copy frame data out of the request.
        if let Some(fb) = req.buffer::<MemoryMappedFrameBuffer<FrameBuffer>>(&stream_clone) {
            let planes = fb.data();
            if let Some(plane) = planes.get(0) {
                *latest_frame_clone.lock().unwrap() = plane.to_vec();
            }
        }
        // Hand the request back for re-queueing.
        req_tx.send(req).ok();
    });

    // Build capture requests and queue them all.
    let mut reqs: Vec<_> = buffers
        .into_iter()
        .map(|buf| {
            let mut req = cam.create_request(None).expect("failed to create request");
            req.add_buffer(&stream, buf)
                .expect("failed to add buffer to request");
            req
        })
        .collect();

    cam.start(None).expect("failed to start camera");
    while let Some(req) = reqs.pop() {
        cam.queue_request(req).expect("failed to queue request");
    }

    // Set up GPIO pin 21 interrupt (short to ground toggles power).
    let (gpio_tx, gpio_rx) = mpsc::channel();
    match rppal::gpio::Gpio::new() {
        Ok(gpio) => match gpio.get(21) {
            Ok(pin) => {
                let mut pin = pin.into_input_pullup();
                pin.set_interrupt(
                    rppal::gpio::Trigger::FallingEdge,
                    Some(Duration::from_millis(200)),
                )
                .unwrap_or_else(|e| {
                    eprintln!("Warning: Failed to set GPIO interrupt on pin 21: {e}")
                });
                println!("GPIO pin 21 interrupt armed (short to ground = power toggle)");
                std::thread::spawn(move || loop {
                    if pin.poll_interrupt(true, None).unwrap().is_some() {
                        if gpio_tx.send(()).is_err() {
                            break;
                        }
                    }
                });
            }
            Err(e) => eprintln!("Warning: Failed to access GPIO pin 21: {e}"),
        },
        Err(e) => eprintln!("Warning: Failed to initialize GPIO: {e}"),
    }

    let settings = Settings {
        orientation: match config.orientation.as_str() {
            "portrait" => Orientation::Portrait,
            _ => Orientation::Landscape,
        },
        flip_vertical: config.flip_vertical,
        flip_horizontal: config.flip_horizontal,
    };

    let event_loop = EventLoop::new().expect(
        "Failed to create event loop — is WAYLAND_DISPLAY or DISPLAY set?\n\
         Run 'echo $WAYLAND_DISPLAY $DISPLAY' to check.",
    );
    let mut app = App {
        _mgr: PhantomData,
        cam,
        latest_frame,
        req_rx,
        actual_size,
        window_size: None,
        window: None,
        context: None,
        surface: None,
        config,
        settings,
        menu_visible: false,
        powered_off: false,
        last_activity: Instant::now(),
        last_pointer_pos: None,
        status_msg: None,
        status_until: None,
        gpio_rx,
    };

    event_loop.run_app(&mut app).unwrap();
}
