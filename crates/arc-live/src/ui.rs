use std::sync::{Arc, RwLock};
use std::time::Duration;

use arc_live_collector::{CollectorEvent, ProbePayload};
use arc_live_core::config::AppConfig;
use arc_live_core::paths::AppPaths;
use arc_live_core::state::{AppState, CollectorPhase, OverlayStats};
use arc_live_core::widget_config::WidgetConfig;
use arc_live_storage::{
    Observation, PersistedStreamSession, STREAM_SESSION_SCHEMA_VERSION, Storage, UserEvent,
};
use chrono::{DateTime, Local, Utc};
use eframe::egui::{self, Color32, RichText};

use crate::process_monitor::ProcessMonitor;
use crate::server::ServerHandle;
use crate::service_client::CollectorRuntime;
use crate::single_instance::SingleInstanceGuard;
use crate::tray::{TrayAction, TrayController};
use crate::updates::UpdateManager;
use crate::view::{
    self, Action, ConnectionView, EventView, Page, StreamView, UpdateView, ViewModel,
};

pub struct ArcLiveApp {
    paths: AppPaths,
    config: AppConfig,
    widget_config: WidgetConfig,
    storage: Storage,
    state: Arc<RwLock<AppState>>,
    collector: CollectorRuntime,
    server: ServerHandle,
    process_monitor: ProcessMonitor,
    collector_ready: bool,
    collector_privileged: bool,
    updates: UpdateManager,
    tray: Option<TrayController>,
    instance: SingleInstanceGuard,
    session_baseline: Option<OverlayStats>,
    stream_day: String,
    stream_started_at: Option<DateTime<Utc>>,
    user_events: Vec<UserEvent>,
    last_sync_succeeded: bool,
    widget_config_error: Option<String>,
    confirm_new_stream: bool,
    onboarding_step: usize,
    page: Page,
    theme_installed: bool,
    _log_guard: tracing_appender::non_blocking::WorkerGuard,
}

impl ArcLiveApp {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        paths: AppPaths,
        storage: Storage,
        state: Arc<RwLock<AppState>>,
        collector: CollectorRuntime,
        server: ServerHandle,
        log_guard: tracing_appender::non_blocking::WorkerGuard,
        config: AppConfig,
        widget_config: WidgetConfig,
        instance: SingleInstanceGuard,
    ) -> Self {
        let stream_day = Self::today();
        let user_events = storage
            .user_events_for_day(&stream_day, 30)
            .unwrap_or_default();
        let saved_session = storage.load_stream_session().ok().flatten();
        let started_new_day = saved_session
            .as_ref()
            .is_some_and(|session| session.local_day != stream_day);
        let restored = saved_session.filter(|session| session.local_day == stream_day);

        let (session_baseline, stream_started_at) = if let Some(session) = restored.as_ref() {
            let preferences = {
                let current = state.read().expect("state poisoned");
                (
                    current.overlay.preset.clone(),
                    current.overlay.language.clone(),
                    current.overlay.background_preset.clone(),
                    current.overlay.background_color,
                    current.overlay.opacity,
                    current.overlay.background_blur,
                )
            };
            let mut baseline = session.baseline.clone();
            baseline.hydrate_legacy_raw_totals();
            let mut overlay = session.overlay.clone();
            overlay.hydrate_legacy_raw_totals();
            overlay.apply_session_baseline(&baseline);
            widget_config.apply(&mut overlay);
            overlay.mode = "live".to_owned();
            overlay.preset = preferences.0;
            overlay.language = preferences.1;
            overlay.background_preset = preferences.2;
            overlay.background_color = preferences.3;
            overlay.opacity = preferences.4;
            overlay.background_blur = preferences.5;
            let mut current = state.write().expect("state poisoned");
            current.overlay = overlay;
            current.record("success", "Today's stream restored from local storage");
            (Some(baseline), Some(session.started_at))
        } else {
            (None, None)
        };

        let mut updates = UpdateManager::new();
        if config.automatic_updates {
            updates.check(
                config.selected_update_feed_url(),
                config.update_channel.clone(),
            );
        }
        let mut app = Self {
            paths,
            process_monitor: ProcessMonitor::start(config.game_process_names.clone()),
            config,
            widget_config,
            storage,
            state,
            collector,
            server,
            collector_ready: false,
            collector_privileged: false,
            updates,
            tray: TrayController::new().ok(),
            instance,
            session_baseline,
            stream_day,
            stream_started_at,
            user_events,
            last_sync_succeeded: false,
            widget_config_error: None,
            confirm_new_stream: false,
            onboarding_step: 0,
            page: Page::Stream,
            theme_installed: false,
            _log_guard: log_guard,
        };
        if restored.is_some() {
            app.record_user_event("success", "Статистика текущего стрима восстановлена");
        } else if started_new_day {
            app.record_user_event(
                "info",
                "Начался новый день — счётчики стрима начнутся заново",
            );
        }
        app.record_user_event("info", "ARC Live запущена");
        app
    }

    fn today() -> String {
        Local::now().date_naive().to_string()
    }

    fn record_user_event(&mut self, level: &str, message: impl Into<String>) {
        let event = UserEvent {
            at: Utc::now(),
            local_day: self.stream_day.clone(),
            level: level.to_owned(),
            message: message.into(),
        };
        if let Err(error) = self.storage.insert_user_event(&event) {
            self.state
                .write()
                .expect("state poisoned")
                .record("error", format!("Saving user event failed: {error}"));
            return;
        }
        self.user_events.insert(0, event);
        self.user_events.truncate(30);
    }

    fn persist_stream_session(&mut self, overlay: &OverlayStats) {
        let Some(baseline) = self.session_baseline.clone() else {
            return;
        };
        let started_at = *self.stream_started_at.get_or_insert_with(Utc::now);
        let session = PersistedStreamSession {
            schema_version: STREAM_SESSION_SCHEMA_VERSION,
            local_day: self.stream_day.clone(),
            started_at,
            updated_at: Utc::now(),
            baseline,
            overlay: overlay.clone(),
        };
        if let Err(error) = self.storage.save_stream_session(&session) {
            self.state
                .write()
                .expect("state poisoned")
                .record("error", format!("Saving current stream failed: {error}"));
        }
    }

    fn rollover_day_if_needed(&mut self) {
        let today = Self::today();
        if today == self.stream_day {
            return;
        }

        self.stream_day = today;
        self.session_baseline = None;
        self.stream_started_at = None;
        self.user_events = self
            .storage
            .user_events_for_day(&self.stream_day, 30)
            .unwrap_or_default();
        {
            let mut state = self.state.write().expect("state poisoned");
            state.overlay.session_downs = 0;
            state.overlay.session_extractions = 0;
            state.overlay.session_deaths = 0;
            state.overlay.session_loot_value = 0;
            state.overlay.session_money_delta = 0;
            state.overlay.session_raw_totals.clear();
            state.overlay.today_extractions = 0;
            state.overlay.today_deaths = 0;
            state.overlay.today_available = false;
            state.overlay.today_raw_totals.clear();
            self.widget_config.apply(&mut state.overlay);
            state.record(
                "info",
                "A new local day started; stream baseline will be refreshed",
            );
        }
        self.record_user_event(
            "info",
            "Начался новый день — счётчики стрима продолжат работу после синхронизации",
        );
    }

    fn start_new_stream(&mut self) {
        let (baseline, overlay, snapshot) = {
            let mut state = self.state.write().expect("state poisoned");
            let baseline = state.overlay.clone();
            state.overlay.mode = "live".to_owned();
            state.overlay.session_downs = 0;
            state.overlay.session_extractions = 0;
            state.overlay.session_deaths = 0;
            state.overlay.session_loot_value = 0;
            state.overlay.session_money_delta = 0;
            state.overlay.session_raw_totals.clear();
            self.widget_config.apply(&mut state.overlay);
            state.record(
                "info",
                "The stream statistics baseline was reset by the user",
            );
            (baseline, state.overlay.clone(), state.clone())
        };
        self.session_baseline = Some(baseline);
        self.stream_started_at = Some(Utc::now());
        self.confirm_new_stream = false;
        self.persist_stream_session(&overlay);
        self.record_user_event("success", "Статистика стрима сброшена — счётчики обнулены");
        self.server.notify(&snapshot);
    }

    fn select_overlay_preset(&mut self, preset: &str) {
        if !self.widget_config.contains(preset) {
            return;
        }
        let (updated, overlay) = {
            let mut state = self.state.write().expect("state poisoned");
            if state.overlay.preset == preset {
                return;
            }
            state.overlay.preset = preset.to_owned();
            state.record("info", format!("OBS widget preset changed to {preset}"));
            (state.clone(), state.overlay.clone())
        };
        self.server.notify(&updated);
        self.save_overlay_preferences(&overlay);
    }

    /// Rereads widget-config.json on demand so preset edits show up without
    /// waiting for the next game synchronization.
    fn reload_widget_config(&mut self) {
        match WidgetConfig::load_or_create(&self.paths.widget_config) {
            Ok(config) => {
                self.widget_config = config;
                self.widget_config_error = None;
                let updated = {
                    let mut state = self.state.write().expect("state poisoned");
                    self.widget_config.apply(&mut state.overlay);
                    state.record("info", "Widget presets reloaded from disk");
                    state.clone()
                };
                let overlay = updated.overlay.clone();
                self.server.notify(&updated);
                self.save_overlay_preferences(&overlay);
                self.record_user_event("success", "Пресеты перечитаны из файла");
            }
            Err(error) => {
                let message = format!("{error:#}");
                self.state
                    .write()
                    .expect("state poisoned")
                    .record("error", format!("Widget config reload failed: {message}"));
                self.widget_config_error = Some(message);
            }
        }
    }

    fn drain_events(&mut self) {
        let mut changed = false;
        let collector_events = self.collector.events().clone();
        while let Ok(event) = collector_events.try_recv() {
            match event {
                CollectorEvent::Connected {
                    version,
                    privileged_service,
                } => {
                    self.collector_privileged = privileged_service;
                    let mut state = self.state.write().expect("state poisoned");
                    state.record(
                        "success",
                        if privileged_service {
                            format!("Capture service {version} connected")
                        } else {
                            format!("Portable capture {version} started")
                        },
                    );
                }
                CollectorEvent::Status(message) => {
                    let mut state = self.state.write().expect("state poisoned");
                    state.phase = CollectorPhase::Capturing;
                    state.record("info", message);
                }
                CollectorEvent::Stats(stats) => {
                    let stats = *stats;
                    let mut state = self.state.write().expect("state poisoned");
                    state.packets_seen = stats.packets_seen;
                    state.tcp_443_segments = stats.tcp_443_segments;
                    state.tcp_443_to_server = stats.tcp_443_to_server;
                    state.tcp_443_to_client = stats.tcp_443_to_client;
                    state.keylog_entries = stats.keylog_entries;
                    if stats.keylog_entries > 0 {
                        state.launcher_prepared = true;
                    }
                    state.tls_records = stats.tls_records;
                    state.tls_records_to_server = stats.tls_records_to_server;
                    state.tls_records_to_client = stats.tls_records_to_client;
                    state.tls_client_hellos = stats.tls_client_hellos;
                    state.tls_server_hellos = stats.tls_server_hellos;
                    state.tls_keys_established = stats.tls_keys_established;
                    state.tls_client_hellos_with_keys = stats.tls_client_hellos_with_keys;
                    state.tls_key_errors = stats.tls_key_errors;
                    state.tls_decrypt_errors = stats.tls_decrypt_errors;
                    state.last_tls_sni = stats.last_tls_sni;
                    state.last_embark_sni = stats.last_embark_sni;
                    state.regional_api_hosts = stats.regional_api_hosts;
                    state.decrypted_records = stats.decrypted_records;
                    state.observations = stats.observations;
                    state.active_capture_connections = stats.active_connections;
                    state.capture_buffered_bytes = stats.buffered_bytes;
                    state.capture_connections_evicted = stats.connections_evicted;
                }
                CollectorEvent::Ready { stats_stream_ready } => {
                    self.collector_ready = stats_stream_ready;
                    let mut state = self.state.write().expect("state poisoned");
                    state.stats_stream_ready = stats_stream_ready;
                    if self.collector_ready {
                        state.phase = CollectorPhase::TokenReady;
                        state.record("success", "Player statistics connection is ready");
                    }
                }
                CollectorEvent::Observation(value) => {
                    let observation = Observation {
                        id: 0,
                        observed_at: Utc::now(),
                        direction: value["direction"].as_str().unwrap_or("unknown").to_owned(),
                        host: value["host"].as_str().unwrap_or("unknown").to_owned(),
                        method: value["method"].as_str().map(str::to_owned),
                        path: value["path"].as_str().map(str::to_owned),
                        status: value["status"].as_u64().map(|v| v as u16),
                        content_type: value["content_type"].as_str().map(str::to_owned),
                        shape: value["body_shape"].clone(),
                    };
                    if let Err(error) = self.storage.insert_observation(&observation) {
                        let mut state = self.state.write().expect("state poisoned");
                        state.record("error", format!("Saving observation failed: {error}"));
                    }
                }
                CollectorEvent::Probe(payload) => self.apply_probe(*payload),
                CollectorEvent::Error(message) => {
                    let mut state = self.state.write().expect("state poisoned");
                    state.phase = CollectorPhase::Error;
                    state.record("error", message);
                    if self.last_sync_succeeded {
                        drop(state);
                        self.record_user_event(
                            "warning",
                            "Связь со статистикой прервалась — ARC Live переподключится сама",
                        );
                        self.last_sync_succeeded = false;
                    }
                }
                CollectorEvent::Stopped => {
                    let mut state = self.state.write().expect("state poisoned");
                    state.record("warning", "Capture stopped");
                }
            }
            self.state.write().expect("state poisoned").last_update = Utc::now();
            changed = true;
        }
        while let Ok(running) = self.process_monitor.changes.try_recv() {
            let mut user_message = None;
            let mut overlay_to_persist = None;
            {
                let mut state = self.state.write().expect("state poisoned");
                if state.game_running != running {
                    state.game_running = running;
                    if running {
                        state.phase = CollectorPhase::Capturing;
                        state.record("success", "ARC Raiders process detected");
                        user_message = Some((
                            "success",
                            "Игра запущена — подключаем статистику автоматически",
                        ));
                    } else {
                        if state.launcher_prepared {
                            state.phase = CollectorPhase::WaitingForGame;
                        }
                        state.record("info", "ARC Raiders process stopped");
                        user_message =
                            Some(("info", "Игра закрыта — данные текущего стрима сохранены"));
                        overlay_to_persist = Some(state.overlay.clone());
                    }
                    changed = true;
                }
            }
            if let Some((level, message)) = user_message {
                self.record_user_event(level, message);
            }
            if let Some(overlay) = overlay_to_persist {
                self.persist_stream_session(&overlay);
            }
        }
        if changed {
            let snapshot = self.state.read().expect("state poisoned").clone();
            self.server.notify(&snapshot);
        }
    }

    fn apply_probe(&mut self, payload: ProbePayload) {
        let observation = Observation {
            id: 0,
            observed_at: payload.observed_at,
            direction: "game_stats_response".to_owned(),
            host: payload.host.clone(),
            method: Some("POST".to_owned()),
            path: Some("/v1/pioneer/stats/player-v2".to_owned()),
            status: Some(payload.status),
            content_type: payload.content_type,
            shape: payload.shape,
        };
        if let Err(error) = self.storage.insert_observation(&observation) {
            self.state
                .write()
                .expect("state poisoned")
                .record("error", format!("Saving stats sync failed: {error}"));
        }
        let mut overlay = payload.overlay;
        if self.session_baseline.is_none() {
            self.session_baseline = Some(overlay.clone());
            self.stream_started_at = Some(Utc::now());
        }
        let baseline = self
            .session_baseline
            .as_ref()
            .expect("session baseline initialized");
        overlay.apply_session_baseline(baseline);
        match WidgetConfig::load_or_create(&self.paths.widget_config) {
            Ok(config) => {
                self.widget_config = config;
                self.widget_config_error = None;
            }
            Err(error) => {
                let message = format!("{error:#}");
                self.state
                    .write()
                    .expect("state poisoned")
                    .record("error", format!("Widget config reload failed: {message}"));
                self.widget_config_error = Some(message);
            }
        }
        self.widget_config.apply(&mut overlay);
        {
            let mut state = self.state.write().expect("state poisoned");
            overlay.preset = match state.overlay.preset.as_str() {
                "session" | "outcome" | "pve" | "pvp" => state.overlay.preset.clone(),
                _ => "account".to_owned(),
            };
            overlay.language = match state.overlay.language.as_str() {
                "en" => "en".to_owned(),
                _ => "ru".to_owned(),
            };
            overlay.opacity = state.overlay.opacity.min(100);
            overlay.background_preset = state.overlay.background_preset.clone();
            overlay.background_color = state.overlay.background_color;
            overlay.background_blur = state.overlay.background_blur.min(20);
            state.overlay = overlay.clone();
            let stats_rows = state.overlay.stats_rows;
            state.record(
                "success",
                format!(
                    "Game stats received on lobby return (HTTP {}, {} rows, {} currently unmapped)",
                    payload.status, stats_rows, payload.unknown_event_rows
                ),
            );
        }
        self.persist_stream_session(&overlay);
        self.record_user_event(
            "success",
            if self.last_sync_succeeded {
                "Статистика обновлена после возвращения в Сперанцу"
            } else {
                "Статистика подключена и текущий стрим восстановлен"
            },
        );
        self.last_sync_succeeded = true;
    }

    fn save_overlay_preferences(&mut self, overlay: &arc_live_core::state::OverlayStats) {
        self.config.overlay_preset = overlay.preset.clone();
        self.config.overlay_language = overlay.language.clone();
        self.config.overlay_background_preset = overlay.background_preset.clone();
        self.config.overlay_background_color = overlay.background_color;
        self.config.overlay_opacity = overlay.opacity;
        self.config.overlay_blur = overlay.background_blur;
        if let Err(error) = self.config.save(&self.paths.config) {
            self.state
                .write()
                .expect("state poisoned")
                .record("error", format!("Saving widget settings failed: {error:#}"));
        }
    }

    fn onboarding_ui(&mut self, root: &mut egui::Ui) {
        egui::CentralPanel::default().show(root, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(42.0);
                ui.heading(RichText::new("Добро пожаловать в ARC Live").size(32.0).strong());
                ui.label(
                    RichText::new("Настроим статистику для OBS за пару минут")
                        .size(17.0)
                        .color(Color32::GRAY),
                );
                ui.add_space(28.0);
            });

            egui::Frame::group(ui.style())
                .inner_margin(egui::Margin::same(22))
                .show(ui, |ui| match self.onboarding_step {
                    0 => {
                        ui.heading("Всё остаётся на этом компьютере");
                        ui.add_space(8.0);
                        ui.label("ARC Live пассивно читает сетевые ответы игры, чтобы посчитать статистику. Приложение не меняет трафик, не управляет игрой и не отправляет токены или TLS-ключи наружу.");
                        ui.add_space(14.0);
                        ui.colored_label(
                            Color32::from_rgb(83, 224, 161),
                            "● История, настройки и виджеты хранятся локально",
                        );
                        ui.add_space(20.0);
                        if ui.button("Продолжить").clicked() {
                            self.onboarding_step = 1;
                        }
                    }
                    1 => {
                        ui.heading("Проверяем подключение");
                        ui.add_space(8.0);
                        if self.collector_privileged {
                            ui.colored_label(
                                Color32::from_rgb(83, 224, 161),
                                "● Фоновый компонент работает",
                            );
                            ui.label("Теперь ARC Live можно запускать без прав администратора.");
                        } else {
                            ui.horizontal(|ui| {
                                ui.spinner();
                                ui.label("Подключаем фоновый компонент…");
                            });
                            ui.label(
                                RichText::new("В portable-версии может потребоваться запуск от администратора. Установленная версия делает это автоматически.")
                                    .color(Color32::GRAY),
                            );
                        }
                        ui.add_space(20.0);
                        ui.horizontal(|ui| {
                            if ui.button("Назад").clicked() {
                                self.onboarding_step = 0;
                            }
                            if ui.button("Дальше").clicked() {
                                self.onboarding_step = 2;
                            }
                        });
                    }
                    _ => {
                        ui.heading("Добавь виджет в OBS");
                        ui.add_space(8.0);
                        let overlay_url = self
                            .state
                            .read()
                            .expect("state poisoned")
                            .local_url
                            .clone()
                            + "/overlay/live";
                        ui.label("Создай Browser Source размером 700 × 80 и вставь эту ссылку:");
                        ui.monospace(&overlay_url);
                        ui.add_space(12.0);
                        ui.horizontal_wrapped(|ui| {
                            if ui.button("Скопировать ссылку").clicked() {
                                ui.ctx().copy_text(overlay_url.clone());
                            }
                            if ui.button("Открыть превью").clicked() {
                                let _ = open::that(&overlay_url);
                            }
                        });
                        ui.add_space(20.0);
                        ui.horizontal(|ui| {
                            if ui.button("Назад").clicked() {
                                self.onboarding_step = 1;
                            }
                            if ui.button("Готово").clicked() {
                                self.config.onboarding_completed = true;
                                if let Err(error) = self.config.save(&self.paths.config) {
                                    self.state.write().expect("state poisoned").record(
                                        "error",
                                        format!("Saving onboarding state failed: {error:#}"),
                                    );
                                }
                            }
                        });
                    }
                });
        });
    }
}

#[allow(dead_code)]
impl ArcLiveApp {
    fn apply(&mut self, action: Action, ctx: &egui::Context) {
        match action {
            Action::Goto(page) => self.page = page,
            Action::SelectPreset(id) => self.select_overlay_preset(&id),
            Action::AskReset => self.confirm_new_stream = true,
            Action::CancelReset => self.confirm_new_stream = false,
            Action::ConfirmReset => self.start_new_stream(),
            Action::CopyObsUrl => {
                let url = self.overlay_url();
                ctx.copy_text(url);
            }
            Action::OpenObsPreview => {
                let _ = open::that(self.overlay_url());
            }
            Action::OpenPresetFile => {
                let _ = open::that(&self.paths.widget_config);
            }
            Action::ReloadPresets => self.reload_widget_config(),
            Action::OpenDataFolder => {
                let _ = open::that(&self.paths.root);
            }
            Action::ExportDiagnostics => self.export_diagnostics(),
            Action::SetLanguage(language) => {
                self.change_appearance(|overlay| overlay.language = language.clone());
            }
            Action::SetAppearance {
                preset,
                color,
                opacity,
                blur,
            } => self.change_appearance(|overlay| {
                overlay.background_preset = preset.clone();
                overlay.background_color = color;
                overlay.opacity = opacity.min(100);
                overlay.background_blur = blur.min(20);
            }),
            Action::SetDemo(true) => self.load_demo_stats(),
            Action::SetDemo(false) => self.clear_demo_stats(),
            Action::FlipDemoBalance => {
                let updated = {
                    let mut state = self.state.write().expect("state poisoned");
                    state.overlay.session_money_delta = if state.overlay.session_money_delta < 0 {
                        72_300
                    } else {
                        -56_100
                    };
                    self.widget_config.apply(&mut state.overlay);
                    state.record("info", "OBS demo balance sign changed");
                    state.clone()
                };
                self.server.notify(&updated);
            }
            Action::SetAutoUpdates(enabled) => {
                self.config.automatic_updates = enabled;
                let _ = self.config.save(&self.paths.config);
            }
            Action::SetChannel(channel) => {
                self.config.update_channel = channel;
                self.updates.available = None;
                self.updates.downloaded = None;
                let _ = self.config.save(&self.paths.config);
            }
            Action::CheckUpdates => {
                let feed = self.config.selected_update_feed_url();
                self.updates.check(feed, self.config.update_channel.clone());
            }
            Action::DownloadUpdate => self.updates.download(self.paths.updates.clone()),
            Action::InstallUpdate => match self.updates.launch_downloaded_after_exit() {
                Ok(()) => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
                Err(error) => self.updates.error = Some(format!("{error:#}")),
            },
        }
    }

    fn overlay_url(&self) -> String {
        let base = self.state.read().expect("state poisoned").local_url.clone();
        format!("{base}/overlay/live")
    }

    fn change_appearance(&mut self, edit: impl FnOnce(&mut OverlayStats)) {
        let (updated, overlay) = {
            let mut state = self.state.write().expect("state poisoned");
            edit(&mut state.overlay);
            state.record("info", "Widget appearance changed");
            (state.clone(), state.overlay.clone())
        };
        self.server.notify(&updated);
        self.save_overlay_preferences(&overlay);
    }

    fn export_diagnostics(&mut self) {
        let snapshot = self.state.read().expect("state poisoned").clone();
        match arc_live_diagnostics::export(&self.paths, &snapshot, &self.storage) {
            Ok(path) => {
                self.state.write().expect("state poisoned").record(
                    "success",
                    format!("Diagnostics exported: {}", path.display()),
                );
                let _ = open::that(path.parent().unwrap_or(&path));
            }
            Err(error) => self
                .state
                .write()
                .expect("state poisoned")
                .record("error", format!("Diagnostics export failed: {error:#}")),
        }
    }

    fn load_demo_stats(&mut self) {
        let updated = {
            let mut state = self.state.write().expect("state poisoned");
            state.overlay.mode = "demo".to_owned();
            state.overlay.eliminations = 24;
            state.overlay.downs = 24;
            state.overlay.raider_damage = 28_640;
            state.overlay.loot_value = 428_750;
            state.overlay.session_downs = 6;
            state.overlay.session_extractions = 3;
            state.overlay.session_deaths = 2;
            state.overlay.session_loot_value = 128_400;
            state.overlay.session_money_delta = -56_100;
            state.overlay.today_extractions = 5;
            state.overlay.today_deaths = 3;
            state.overlay.today_available = true;
            state
                .overlay
                .raw_totals
                .insert("event.200.target.995408715".into(), 24);
            state.overlay.raw_totals.insert("event.101".into(), 28_640);
            state
                .overlay
                .session_raw_totals
                .insert("event.200.target.995408715".into(), 6);
            state
                .overlay
                .session_raw_totals
                .insert("event.101".into(), 55_600);
            state
                .overlay
                .session_raw_totals
                .insert("event.101.target.995408715".into(), 18_700);
            state
                .overlay
                .session_raw_totals
                .insert("event.101.target.200993951".into(), 900);
            self.widget_config.apply(&mut state.overlay);
            state.record("info", "OBS demo statistics loaded");
            state.clone()
        };
        self.server.notify(&updated);
    }

    fn clear_demo_stats(&mut self) {
        let updated = {
            let mut state = self.state.write().expect("state poisoned");
            let appearance = state.overlay.clone();
            state.overlay = OverlayStats {
                mode: "live".to_owned(),
                preset: appearance.preset,
                language: appearance.language,
                background_preset: appearance.background_preset,
                background_color: appearance.background_color,
                opacity: appearance.opacity,
                background_blur: appearance.background_blur,
                ..Default::default()
            };
            self.widget_config.apply(&mut state.overlay);
            state.record("info", "OBS demo statistics cleared");
            state.clone()
        };
        self.server.notify(&updated);
    }
}

impl eframe::App for ArcLiveApp {
    fn ui(&mut self, root: &mut egui::Ui, _frame: &mut eframe::Frame) {
        if !self.theme_installed {
            view::install_theme(root.ctx());
            self.theme_installed = true;
        }
        // A second launch hands activation to the running instance instead of
        // starting a new one; bring the window back to the front.
        while self.instance.activation.try_recv().is_ok() {
            root.ctx()
                .send_viewport_cmd(egui::ViewportCommand::Visible(true));
            root.ctx().send_viewport_cmd(egui::ViewportCommand::Focus);
        }
        if let Some(action) = self.tray.as_ref().and_then(TrayController::poll) {
            match action {
                TrayAction::Show => {
                    root.ctx()
                        .send_viewport_cmd(egui::ViewportCommand::Visible(true));
                    root.ctx().send_viewport_cmd(egui::ViewportCommand::Focus);
                }
                TrayAction::Exit => {
                    root.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        }
        self.updates.drain();
        if !self.config.onboarding_completed {
            self.drain_events();
            self.onboarding_ui(root);
            root.ctx().request_repaint_after(Duration::from_millis(250));
            return;
        }
        self.rollover_day_if_needed();
        self.drain_events();
        root.ctx().request_repaint_after(Duration::from_millis(250));

        let snapshot = self.state.read().expect("state poisoned").clone();
        let events: Vec<EventView> = self
            .user_events
            .iter()
            .take(8)
            .map(|event| EventView {
                time: event.at.with_timezone(&Local).format("%H:%M").to_string(),
                level: event.level.clone(),
                message: event.message.clone(),
            })
            .collect();
        let overlay_url = format!("{}/overlay/live", snapshot.local_url);
        let actions = {
            let model = ViewModel {
                page: self.page,
                version: env!("CARGO_PKG_VERSION"),
                overlay: &snapshot.overlay,
                connection: ConnectionView {
                    game_running: snapshot.game_running,
                    stats_ready: self.collector_ready,
                    launcher_prepared: snapshot.launcher_prepared,
                    service_privileged: self.collector_privileged,
                },
                stream: StreamView {
                    started_at: self.stream_started_at.map(|at| {
                        at.with_timezone(&Local)
                            .format("сегодня с %H:%M")
                            .to_string()
                    }),
                    can_reset: snapshot.overlay.stats_rows > 0 && snapshot.overlay.mode != "demo",
                    confirm_reset: self.confirm_new_stream,
                },
                events: &events,
                obs_url: &overlay_url,
                updates: UpdateView {
                    automatic: self.config.automatic_updates,
                    channel: self.config.update_channel.clone(),
                    checking: self.updates.checking,
                    downloading: self.updates.downloading,
                    available: self
                        .updates
                        .available
                        .as_ref()
                        .map(|manifest| manifest.version.clone()),
                    downloaded: self.updates.downloaded.is_some(),
                    blocked_by_game: snapshot.game_running,
                    error: self.updates.error.clone(),
                },
                preset_error: self.widget_config_error.as_deref(),
            };
            view::render(root, &model)
        };
        let ctx = root.ctx().clone();
        for action in actions {
            self.apply(action, &ctx);
        }
    }
}

impl Drop for ArcLiveApp {
    fn drop(&mut self) {
        let overlay = self.state.read().expect("state poisoned").overlay.clone();
        if overlay.mode != "demo" {
            self.persist_stream_session(&overlay);
        }
        self.record_user_event("info", "ARC Live закрыта — данные стрима сохранены");
        self.collector.stop();
    }
}
