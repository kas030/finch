#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod gpu_tiles;
mod library_panel;
mod platform;
mod plugin_panel;
mod reader;
mod services;
mod viewport;

use app::{App, Message};
use platform::{Config, Instance, InstanceClaim};

fn main() {
    let started = std::time::Instant::now();
    if let Err(error) = run(started) {
        eprintln!("{error}");
        if error != platform::HELP {
            rfd::MessageDialog::new()
                .set_title("Finch")
                .set_description(error)
                .set_level(rfd::MessageLevel::Error)
                .show();
            std::process::exit(1);
        }
    }
}
fn run(started: std::time::Instant) -> Result<(), String> {
    let config = Config::parse()?;
    // This runs before any thread is created, making the renderer override safe.
    unsafe {
        std::env::set_var(
            "ICED_BACKEND",
            if config.software { "tiny-skia" } else { "wgpu" },
        );
    }
    let paths = config.paths().map_err(|e| e.to_string())?;
    let instance = match Instance::claim(&paths, config.file.clone())? {
        InstanceClaim::Primary(instance) => instance,
        InstanceClaim::Forwarded => return Ok(()),
    };
    let app = std::cell::RefCell::new(Some(App::new(config, paths, instance, started)?));
    iced::application(
        move || app.borrow_mut().take().expect("boot once"),
        App::update,
        App::view,
    )
    .title(App::title)
    .theme(App::theme)
    .subscription(App::subscription)
    .executor::<services::SmallExecutor>()
    .window_size((1280.0, 860.0))
    .exit_on_close_request(false)
    .settings(iced::Settings {
        default_text_size: iced::Pixels(14.0),
        ..Default::default()
    })
    .run()
    .map_err(|e| e.to_string())
}
