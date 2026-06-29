use std::{
    cell::RefCell,
    path::PathBuf,
    rc::Rc,
    sync::{Arc, Mutex, mpsc},
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
use views::{Settings, View, slint_view::SlintView};

mod views;

const PIXEL_FORMAT_RGB888: PixelFormat = PixelFormat::new(DrmFourcc::Rgb888 as u32, 0);

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
}

/// Messages sent from the concrete view back to the application logic.
enum UiCommand {
    ShowMenu,
    DismissMenu,
    Wake,
    PowerOff,
    TakePhoto,
}

/// Holds the runtime state that drives the view.
struct AppState<'a> {
    cam: libcamera::camera::ActiveCamera<'a>,
    latest_frame: Arc<Mutex<Vec<u8>>>,
    req_rx: mpsc::Receiver<libcamera::request::Request>,
    cmd_rx: mpsc::Receiver<UiCommand>,
    actual_size: Size,
    config: Config,
    settings: Settings,
    menu_visible: bool,
    powered_off: bool,
    last_activity: Instant,
    status_msg: Option<String>,
    status_until: Option<Instant>,
    gpio_rx: mpsc::Receiver<()>,
}

impl<'a> AppState<'a> {
    fn show_menu(&mut self) {
        self.menu_visible = true;
    }

    fn dismiss_menu(&mut self) {
        self.menu_visible = false;
    }

    fn wake(&mut self) {
        self.powered_off = false;
    }

    fn power_off(&mut self) {
        self.powered_off = true;
        self.menu_visible = false;
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

    fn save_photo(&mut self) {
        let expected = (self.actual_size.width as usize) * (self.actual_size.height as usize) * 3;

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

        let img = match image::RgbImage::from_raw(
            self.actual_size.width as u32,
            self.actual_size.height as u32,
            frame_data,
        ) {
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

    /// Single step of the main loop: process input, requeue camera requests,
    /// check timeouts, and refresh the view.
    fn tick(&mut self, view: &dyn View) {
        while let Ok(cmd) = self.cmd_rx.try_recv() {
            self.last_activity = Instant::now();
            match cmd {
                UiCommand::ShowMenu => self.show_menu(),
                UiCommand::DismissMenu => self.dismiss_menu(),
                UiCommand::Wake => self.wake(),
                UiCommand::PowerOff => self.power_off(),
                UiCommand::TakePhoto => self.save_photo(),
            }
        }

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

        // Expire transient status messages.
        if let Some(until) = self.status_until {
            if Instant::now() > until {
                self.status_msg = None;
                self.status_until = None;
            }
        }

        // Push the latest state to the view.
        view.set_powered_off(self.powered_off);
        view.set_menu_visible(self.menu_visible);
        view.set_settings(&self.settings);
        view.set_status(self.status_msg.clone());

        let frame = self.latest_frame.lock().unwrap();
        view.update_frame(&frame, self.actual_size);
    }
}

fn main() {
    let config = load_config();
    println!("Photo directory: {}", config.photo_directory.display());

    let size = Size {
        width: 1280,
        height: 720,
    };

    // Leak the camera manager so the active camera can be used from 'static
    // closures while the application runs.
    let mgr = Box::leak(Box::new(CameraManager::new().unwrap()));
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
                std::thread::spawn(move || {
                    loop {
                        if pin.poll_interrupt(true, None).unwrap().is_some() {
                            if gpio_tx.send(()).is_err() {
                                break;
                            }
                        }
                    }
                });
            }
            Err(e) => eprintln!("Warning: Failed to access GPIO pin 21: {e}"),
        },
        Err(e) => eprintln!("Warning: Failed to initialize GPIO: {e}"),
    }

    let settings = Settings;

    // Create the view and wire up its user-input callbacks.
    let view: Rc<dyn View> = Rc::new(SlintView::new());
    let (cmd_tx, cmd_rx) = mpsc::channel::<UiCommand>();

    view.on_show_menu(Box::new({
        let cmd_tx = cmd_tx.clone();
        move || {
            cmd_tx.send(UiCommand::ShowMenu).ok();
        }
    }));
    view.on_dismiss_menu(Box::new({
        let cmd_tx = cmd_tx.clone();
        move || {
            cmd_tx.send(UiCommand::DismissMenu).ok();
        }
    }));
    view.on_wake(Box::new({
        let cmd_tx = cmd_tx.clone();
        move || {
            cmd_tx.send(UiCommand::Wake).ok();
        }
    }));
    view.on_power_off(Box::new({
        let cmd_tx = cmd_tx.clone();
        move || {
            cmd_tx.send(UiCommand::PowerOff).ok();
        }
    }));
    view.on_take_photo(Box::new({
        let cmd_tx = cmd_tx.clone();
        move || {
            cmd_tx.send(UiCommand::TakePhoto).ok();
        }
    }));

    let app = RefCell::new(AppState {
        cam,
        latest_frame,
        req_rx,
        cmd_rx,
        actual_size,
        config,
        settings,
        menu_visible: false,
        powered_off: false,
        last_activity: Instant::now(),
        status_msg: None,
        status_until: None,
        gpio_rx,
    });

    let view_for_timer = view.clone();
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(16),
        move || {
            app.borrow_mut().tick(&*view_for_timer);
        },
    );

    view.run().expect("event loop failed");
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
