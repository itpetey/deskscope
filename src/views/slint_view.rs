use std::sync::{Arc, Mutex};

use libcamera::geometry::Size;
use slint::ComponentHandle;
use ui::DeskscopeWindow;

use crate::views::{Settings, View};

mod ui {
    slint::include_modules!();
}

pub struct SlintView {
    window: DeskscopeWindow,
    settings: Arc<Mutex<Settings>>,
    callbacks: Arc<Mutex<Callbacks>>,
}

struct Callbacks {
    show_menu: Option<Box<dyn Fn() + Send + Sync>>,
    dismiss_menu: Option<Box<dyn Fn() + Send + Sync>>,
    wake: Option<Box<dyn Fn() + Send + Sync>>,
    power_off: Option<Box<dyn Fn() + Send + Sync>>,
    take_photo: Option<Box<dyn Fn() + Send + Sync>>,
}

impl SlintView {
    pub fn new() -> Self {
        let window = DeskscopeWindow::new().expect("failed to create Slint window");

        let settings = Arc::new(Mutex::new(Settings));

        let callbacks = Arc::new(Mutex::new(Callbacks {
            show_menu: None,
            dismiss_menu: None,
            wake: None,
            power_off: None,
            take_photo: None,
        }));

        let cbs = callbacks.clone();
        window.on_show_menu(move || invoke(&cbs, |c| c.show_menu.as_ref()));

        let cbs = callbacks.clone();
        window.on_dismiss_menu(move || invoke(&cbs, |c| c.dismiss_menu.as_ref()));

        let cbs = callbacks.clone();
        window.on_wake(move || invoke(&cbs, |c| c.wake.as_ref()));

        let cbs = callbacks.clone();
        window.on_power_off(move || invoke(&cbs, |c| c.power_off.as_ref()));

        let cbs = callbacks.clone();
        window.on_take_photo(move || invoke(&cbs, |c| c.take_photo.as_ref()));

        Self {
            window,
            settings,
            callbacks,
        }
    }
}

impl View for SlintView {
    fn update_frame(&self, frame: &[u8], actual_size: Size) {
        let size = self.window.window().size();
        let win_w = size.width as usize;
        let win_h = size.height as usize;
        if win_w == 0 || win_h == 0 {
            return;
        }

        let mut rgba = vec![0u8; win_w * win_h * 4];
        fill_with_black(&mut rgba);

        if !frame.is_empty() {
            render_frame(frame, actual_size, win_w, win_h, &mut rgba);
        }

        let pixel_buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
            &rgba,
            win_w as u32,
            win_h as u32,
        );
        let image = slint::Image::from_rgba8(pixel_buffer);
        self.window.set_camera_image(image);
    }

    fn set_menu_visible(&self, visible: bool) {
        self.window.set_menu_visible(visible);
    }

    fn set_powered_off(&self, off: bool) {
        self.window.set_powered_off(off);
    }

    fn set_status(&self, msg: Option<String>) {
        self.window.set_status_text(msg.unwrap_or_default().into());
    }

    fn set_settings(&self, settings: &Settings) {
        *self.settings.lock().unwrap() = settings.clone();
    }

    fn run(&self) -> Result<(), Box<dyn std::error::Error>> {
        self.window
            .run()
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error>)
    }

    fn on_show_menu(&self, callback: Box<dyn Fn() + Send + Sync>) {
        self.callbacks.lock().unwrap().show_menu = Some(callback);
    }

    fn on_dismiss_menu(&self, callback: Box<dyn Fn() + Send + Sync>) {
        self.callbacks.lock().unwrap().dismiss_menu = Some(callback);
    }

    fn on_wake(&self, callback: Box<dyn Fn() + Send + Sync>) {
        self.callbacks.lock().unwrap().wake = Some(callback);
    }

    fn on_power_off(&self, callback: Box<dyn Fn() + Send + Sync>) {
        self.callbacks.lock().unwrap().power_off = Some(callback);
    }

    fn on_take_photo(&self, callback: Box<dyn Fn() + Send + Sync>) {
        self.callbacks.lock().unwrap().take_photo = Some(callback);
    }
}

fn fill_with_black(rgba: &mut [u8]) {
    for chunk in rgba.chunks_exact_mut(4) {
        chunk[0] = 0;
        chunk[1] = 0;
        chunk[2] = 0;
        chunk[3] = 255;
    }
}

fn invoke<F>(callbacks: &Arc<Mutex<Callbacks>>, selector: F)
where
    F: FnOnce(&Callbacks) -> Option<&Box<dyn Fn() + Send + Sync>>,
{
    let guard = callbacks.lock().unwrap();
    if let Some(cb) = selector(&guard) {
        cb();
    }
}

fn render_frame(frame: &[u8], actual_size: Size, win_w: usize, win_h: usize, rgba: &mut [u8]) {
    let cam_w = actual_size.width as usize;
    let cam_h = actual_size.height as usize;

    if frame.len() < cam_w * cam_h * 3 {
        return;
    }

    let virt_w = cam_w as f32;
    let virt_h = cam_h as f32;
    let scale_x = win_w as f32 / virt_w;
    let scale_y = win_h as f32 / virt_h;
    let scale = scale_x.min(scale_y);

    let scaled_w = (virt_w * scale) as usize;
    let scaled_h = (virt_h * scale) as usize;
    let dst_off_x = (win_w - scaled_w) / 2;
    let dst_off_y = (win_h - scaled_h) / 2;

    if scaled_w == 0 || scaled_h == 0 {
        return;
    }

    for dy in 0..scaled_h {
        let vy = (dy as f32 / scale) as usize;
        if vy >= cam_h {
            continue;
        }

        for dx in 0..scaled_w {
            let vx = (dx as f32 / scale) as usize;
            if vx >= cam_w {
                continue;
            }

            let src = (vy * cam_w + vx) * 3;
            let dst = ((dst_off_y + dy) * win_w + (dst_off_x + dx)) * 4;
            rgba[dst] = frame[src];
            rgba[dst + 1] = frame[src + 1];
            rgba[dst + 2] = frame[src + 2];
            rgba[dst + 3] = 255;
        }
    }
}
