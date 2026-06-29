use libcamera::geometry::Size;

pub mod slint_view;

/// Abstract interface for the deskscope user-interface.
///
/// The application logic in `main.rs` operates against this trait so the
/// concrete rendering toolkit (Slint, in this implementation) is isolated in
/// the `views` module.
pub trait View {
    /// Render a new camera frame.
    fn update_frame(&self, frame: &[u8], actual_size: Size);

    /// Show or hide the settings menu.
    fn set_menu_visible(&self, visible: bool);

    /// Show or hide the power-off overlay.
    fn set_powered_off(&self, off: bool);

    /// Display a transient status message, or clear it.
    fn set_status(&self, msg: Option<String>);

    /// Update the on-screen settings labels.
    fn set_settings(&self, settings: &Settings);

    /// Run the view's event loop.
    fn run(&self) -> Result<(), Box<dyn std::error::Error>>;

    // Callbacks the view fires in response to user input.
    fn on_show_menu(&self, callback: Box<dyn Fn() + Send + Sync>);
    fn on_dismiss_menu(&self, callback: Box<dyn Fn() + Send + Sync>);
    fn on_wake(&self, callback: Box<dyn Fn() + Send + Sync>);
    fn on_power_off(&self, callback: Box<dyn Fn() + Send + Sync>);
    fn on_take_photo(&self, callback: Box<dyn Fn() + Send + Sync>);
}

/// Display settings controlled through the UI.
#[derive(Debug, Clone)]
pub struct Settings;
