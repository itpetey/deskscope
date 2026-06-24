use std::{
    marker::PhantomData,
    num::NonZeroU32,
    sync::{Arc, Mutex, mpsc},
};

use drm_fourcc::DrmFourcc;
use libcamera::{
    camera::CameraConfigurationStatus,
    camera_manager::CameraManager,
    framebuffer_allocator::{FrameBuffer, FrameBufferAllocator},
    framebuffer_map::MemoryMappedFrameBuffer,
    geometry::Size,
    pixel_format::PixelFormat,
    properties,
    stream::StreamRole,
};
use winit::{
    application::ApplicationHandler,
    event::WindowEvent,
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop},
    window::{Fullscreen, Window, WindowId},
};

const PIXEL_FORMAT_RGB888: PixelFormat = PixelFormat::new(DrmFourcc::Rgb888 as u32, 0);

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
            _ => {}
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        // Re-queue completed requests so the camera keeps streaming
        while let Ok(req) = self.req_rx.try_recv() {
            self.cam.queue_request(req).ok();
        }

        let Some(surface) = self.surface.as_mut() else {
            return;
        };
        let Some(window_size) = self.window_size else {
            return;
        };

        let frame = self.latest_frame.lock().unwrap();
        if frame.is_empty() {
            return;
        }

        if let Ok(mut buffer) = surface.buffer_mut() {
            let cam_w = self.actual_size.width as usize;
            let cam_h = self.actual_size.height as usize;
            let win_w = window_size.width as usize;
            let win_h = window_size.height as usize;

            // Fill any uncovered area with black
            buffer.fill(0xFF00_0000);

            let copy_w = cam_w.min(win_w);
            let copy_h = cam_h.min(win_h);

            for y in 0..copy_h {
                for x in 0..copy_w {
                    let src = (y * cam_w + x) * 3;
                    let dst = y * win_w + x;

                    let r = frame[src];
                    let g = frame[src + 1];
                    let b = frame[src + 2];

                    // BGRA (little-endian) — softbuffer native format
                    buffer[dst] = (255u32 << 24)
                        | ((r as u32) << 16)
                        | ((g as u32) << 8)
                        | (b as u32);
                }
            }

            buffer.present().ok();
        }
    }
}

fn main() {
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

    // Allocate and map frame buffers
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

    // Shared latest frame between camera callback and rendering thread
    let latest_frame: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));

    // Channel to return completed requests for re-queueing
    let (req_tx, req_rx) = mpsc::channel();

    let stream_clone = stream.clone();
    let latest_frame_clone = Arc::clone(&latest_frame);

    // Register callback: fires on libcamera's internal thread when a request completes
    cam.on_request_completed(move |req| {
        // Copy frame data out of the request
        if let Some(fb) = req.buffer::<MemoryMappedFrameBuffer<FrameBuffer>>(&stream_clone) {
            let planes = fb.data();
            if let Some(plane) = planes.get(0) {
                *latest_frame_clone.lock().unwrap() = plane.to_vec();
            }
        }
        // Hand the request back for re-queueing
        req_tx.send(req).ok();
    });

    // Build capture requests and queue them all
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

    let event_loop = EventLoop::new().unwrap();
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
    };

    event_loop.run_app(&mut app).unwrap();
}
