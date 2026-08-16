#![cfg_attr(windows, windows_subsystem = "windows")]

mod branding;
mod process_env;
mod process_monitor;
mod server;
mod service_client;
mod session_setup;
mod single_instance;
mod tray;
mod ui;
use arc_live::view;
mod updates;

use std::fs;
use std::sync::{Arc, RwLock};

use anyhow::Result;
use arc_live_core::config::AppConfig;
use arc_live_core::paths::AppPaths;
use arc_live_core::state::AppState;
use arc_live_core::widget_config::WidgetConfig;
use arc_live_storage::Storage;
use tracing_subscriber::EnvFilter;

fn main() {
    if let Err(error) = run() {
        eprintln!("ARC Live failed: {error:#}");
        #[cfg(windows)]
        {
            let _ = std::process::Command::new("msg.exe")
                .args(["*", &format!("ARC Live failed: {error:#}")])
                .status();
        }
    }
}

fn run() -> Result<()> {
    let Some(instance) = single_instance::acquire()? else {
        return Ok(());
    };
    let paths = AppPaths::discover()?;
    let service_token = paths.load_or_create_service_token()?;
    let mut config = AppConfig::load_or_create(&paths.config)?;
    let (widget_config, widget_config_warning) =
        match WidgetConfig::load_or_create(&paths.widget_config) {
            Ok(config) => (config, None),
            Err(error) => (WidgetConfig::default(), Some(error.to_string())),
        };
    cleanup_old_logs(&paths.logs, "arc-live.log", 7);
    let file_appender = tracing_appender::rolling::daily(&paths.logs, "arc-live.log");
    let (writer, guard) = tracing_appender::non_blocking(file_appender);
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_writer(writer)
        .with_ansi(false)
        .try_init()
        .ok();

    cleanup_old_keylogs(&paths);
    let session_setup = session_setup::configure(&paths)?;
    let keylog_path = session_setup.keylog_path;

    let storage = Storage::open(&paths.database)?;
    let mut initial_state =
        AppState::new(env!("CARGO_PKG_VERSION"), keylog_path.display().to_string());
    initial_state.local_url = format!("http://127.0.0.1:{}", config.local_port);
    initial_state.overlay.preset = config.overlay_preset.clone();
    initial_state.overlay.language = config.overlay_language.clone();
    initial_state.overlay.background_preset = config.overlay_background_preset.clone();
    initial_state.overlay.background_color = config.overlay_background_color;
    initial_state.overlay.opacity = config.overlay_opacity;
    initial_state.overlay.background_blur = config.overlay_blur;
    widget_config.apply(&mut initial_state.overlay);
    initial_state.database_ready = true;
    initial_state.launcher_prepared = session_setup.launcher_ready;
    initial_state.phase = if session_setup.launcher_ready {
        arc_live_core::state::CollectorPhase::WaitingForGame
    } else {
        arc_live_core::state::CollectorPhase::WaitingForLauncher
    };
    initial_state.record("info", "Live Stats build initialized");
    initial_state.record("info", session_setup.status);
    if let Some(warning) = widget_config_warning {
        initial_state.record(
            "warning",
            format!(
                "Using built-in widget mapping because widget-config.json is invalid: {warning}"
            ),
        );
    }
    let shared_state = Arc::new(RwLock::new(initial_state));

    let server = server::start(
        Arc::clone(&shared_state),
        storage.clone(),
        config.local_port,
    )?;
    let actual_local_port = server.port();
    {
        let mut state = shared_state.write().expect("state poisoned");
        state.local_url = format!("http://127.0.0.1:{actual_local_port}");
        if actual_local_port != config.local_port {
            state.record(
                "warning",
                format!(
                    "Local port {} was busy; ARC Live selected and saved {}",
                    config.local_port, actual_local_port
                ),
            );
        }
    }
    if actual_local_port != config.local_port {
        config.local_port = actual_local_port;
        config.save(&paths.config)?;
    }
    let collector = service_client::CollectorRuntime::connect_or_start_local(
        keylog_path.clone(),
        service_token,
    );

    let app = ui::ArcLiveApp::new(
        paths,
        storage,
        shared_state,
        collector,
        server,
        guard,
        config,
        widget_config,
        instance,
    );

    let start_hidden = std::env::args().any(|argument| argument == "--background");
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([920.0, 720.0])
            .with_min_inner_size([720.0, 560.0])
            .with_icon(branding::window_icon())
            .with_visible(!start_hidden),
        ..Default::default()
    };
    eframe::run_native("ARC Live", options, Box::new(|_| Ok(Box::new(app))))
        .map_err(|error| anyhow::anyhow!(error.to_string()))
}

fn cleanup_old_logs(directory: &std::path::Path, prefix: &str, keep_days: u64) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    let cutoff = std::time::SystemTime::now()
        .checked_sub(std::time::Duration::from_secs(keep_days * 24 * 60 * 60));
    for entry in entries.flatten() {
        let path = entry.path();
        let matches = path
            .file_name()
            .and_then(|value| value.to_str())
            .is_some_and(|name| name.starts_with(prefix));
        let old = entry
            .metadata()
            .ok()
            .and_then(|metadata| metadata.modified().ok())
            .zip(cutoff)
            .is_some_and(|(modified, cutoff)| modified < cutoff);
        if matches && old {
            let _ = fs::remove_file(path);
        }
    }
}

fn cleanup_old_keylogs(paths: &AppPaths) {
    let Ok(entries) = fs::read_dir(&paths.sessions) else {
        return;
    };
    let cutoff =
        std::time::SystemTime::now().checked_sub(std::time::Duration::from_secs(24 * 60 * 60));
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("keys") {
            continue;
        }
        if path.file_name().and_then(|value| value.to_str()) == Some("arc-live-tls.keys") {
            continue;
        }
        let old = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .zip(cutoff)
            .is_some_and(|(modified, cutoff)| modified < cutoff);
        if old {
            let _ = fs::remove_file(path);
        }
    }
}
