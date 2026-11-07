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
#[cfg(windows)]
#[path = "spk_router/management.rs"]
mod management;
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

#[cfg(test)]
mod product_definition_tests {
    #[test]
    fn installer_definition_matches_binary_version() {
        let definition: serde_json::Value =
            serde_json::from_str(include_str!("../tool.json")).unwrap();
        assert_eq!(definition["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(definition["version"], crate::config::APP_VERSION);
    }
}
