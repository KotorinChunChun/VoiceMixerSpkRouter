#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#[path = "spk_router/application.rs"]
mod application;
#[path = "spk_router/catalog.rs"]
mod catalog;
#[path = "spk_router/cli.rs"]
mod cli;
#[path = "spk_router/config.rs"]
mod config;
#[path = "spk_router/engine.rs"]
mod engine;
#[cfg(windows)]
#[path = "spk_router/instance.rs"]
mod instance;
#[path = "spk_router/key_binding.rs"]
mod key_binding;
#[path = "spk_router/state.rs"]
mod state;
#[cfg(windows)]
#[path = "spk_router/ui.rs"]
mod ui;

fn run() -> Result<(), String> {
    let default_path = std::env::current_exe()
        .map_err(|e| e.to_string())?
        .with_file_name("spk-settings.json");
    let options = cli::Options::parse(std::env::args().skip(1), default_path)?;
    application::run(options)
}

#[path = "spk_router/telemetry.rs"]
mod telemetry;

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        #[cfg(all(windows, not(debug_assertions)))]
        ui::error(&error);
        std::process::exit(1);
    }
}
