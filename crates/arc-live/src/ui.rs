use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AppPage {
    Home,
    Widget,
    Settings,
}

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
    probing_stats: bool,
    live_stats_enabled: bool,
    next_stats_probe: Instant,
    session_baseline: Option<OverlayStats>,
    stream_day: String,
    stream_started_at: Option<DateTime<Utc>>,
    user_events: Vec<UserEvent>,
    last_sync_succeeded: bool,
    confirm_new_stream: bool,
    onboarding_step: usize,
    page: AppPage,
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
            probing_stats: false,
            live_stats_enabled: true,
            next_stats_probe: Instant::now(),
            session_baseline,
            stream_day,
            stream_started_at,
            user_events,
            last_sync_succeeded: false,
            confirm_new_stream: false,
            onboarding_step: 0,
            page: AppPage::Home,
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
        self.next_stats_probe = Instant::now();
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
        if !matches!(preset, "account" | "session" | "outcome" | "pve" | "pvp") {
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
                    state.decrypted_records = stats.decrypted_records;
                    state.observations = stats.observations;
                    state.active_capture_connections = stats.active_connections;
                    state.capture_buffered_bytes = stats.buffered_bytes;
                    state.capture_connections_evicted = stats.connections_evicted;
                }
                CollectorEvent::Ready {
                    token_seen,
                    request_template_seen,
                } => {
                    self.collector_ready = token_seen && request_template_seen;
                    let mut state = self.state.write().expect("state poisoned");
                    state.token_seen = token_seen;
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
            direction: "active_probe_response".to_owned(),
            host: "api-gateway.europe.es-pio.net".to_owned(),
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
            Ok(config) => self.widget_config = config,
            Err(error) => self
                .state
                .write()
                .expect("state poisoned")
                .record("error", format!("Widget config reload failed: {error:#}")),
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
                    "Live stats synced (HTTP {}, {} rows, {} currently unmapped)",
                    payload.status, stats_rows, payload.unknown_event_rows
                ),
            );
        }
        self.persist_stream_session(&overlay);
        if !self.last_sync_succeeded {
            self.record_user_event(
                "success",
                "Статистика подключена — текущий стрим восстановлен и обновляется",
            );
        }
        self.last_sync_succeeded = true;
    }

    fn probe_stats(&mut self) {}

    fn maybe_probe_stats(&mut self) {
        self.probing_stats = false;
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
    fn ui(&mut self, root: &mut egui::Ui, _frame: &mut eframe::Frame) {
        while self.instance.activation.try_recv().is_ok() {
            root.ctx()
                .send_viewport_cmd(egui::ViewportCommand::Visible(true));
            root.ctx().send_viewport_cmd(egui::ViewportCommand::Focus);
        }
        self.drain_events();
        self.maybe_probe_stats();
        root.ctx()
            .request_repaint_after(std::time::Duration::from_millis(250));
        let snapshot = self.state.read().expect("state poisoned").clone();

        egui::CentralPanel::default().show(root, |ui| {
            ui.heading(RichText::new("ARC Live").size(32.0).strong());
            ui.label(RichText::new("Independent live statistics for OBS").color(Color32::GRAY));
            ui.add_space(12.0);

            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("{:?}", snapshot.phase)).strong());
                    if snapshot.token_seen { ui.colored_label(Color32::from_rgb(83, 224, 161), "Embark token ready"); }
                    else { ui.colored_label(Color32::from_rgb(246, 183, 60), "Waiting for token"); }
                });
                ui.separator();
                ui.columns(3, |columns| {
                    columns[0].label("Captured packets"); columns[0].heading(snapshot.packets_seen.to_string());
                    columns[1].label("Decrypted records"); columns[1].heading(snapshot.decrypted_records.to_string());
                    columns[2].label("Observations"); columns[2].heading(snapshot.observations.to_string());
                });
                ui.separator();
                ui.label(format!(
                    "TCP 443: {}  |  Keylog: {}  |  TLS handshakes: {}/{}  |  Key errors: {}  |  Decrypt errors: {}",
                    snapshot.tcp_443_segments,
                    snapshot.keylog_entries,
                    snapshot.tls_client_hellos,
                    snapshot.tls_server_hellos,
                    snapshot.tls_key_errors,
                    snapshot.tls_decrypt_errors,
                ));
                ui.label(format!(
                    "Direction TCP out/in: {}/{}  |  TLS out/in: {}/{}  |  ClientHello with key: {}  |  Embark: {}",
                    snapshot.tcp_443_to_server,
                    snapshot.tcp_443_to_client,
                    snapshot.tls_records_to_server,
                    snapshot.tls_records_to_client,
                    snapshot.tls_client_hellos_with_keys,
                    snapshot.last_embark_sni.as_deref().unwrap_or("not seen"),
                ));
            });

            ui.add_space(12.0);
            ui.heading("1. Automatic connection");
            ui.label("ARC Live attaches automatically. It never closes or restarts Steam, Epic, or ARC Raiders.");
            if snapshot.launcher_prepared {
                ui.colored_label(Color32::from_rgb(83, 224, 161), "Running launcher is ready");
            } else {
                ui.colored_label(Color32::from_rgb(246, 183, 60), "One-time setup is installed. It will activate after Steam or Epic next starts normally.");
            }

            ui.add_space(12.0);
            ui.heading("2. OBS preview");
            let overlay_url = format!("{}/overlay/live", snapshot.local_url);
            ui.horizontal(|ui| {
                ui.monospace(&overlay_url);
                if ui.button("Open preview").clicked() { let _ = open::that(&overlay_url); }
            });
            ui.horizontal(|ui| {
                if ui.button("Load demo stats").clicked() {
                    let updated = {
                        let mut state = self.state.write().expect("state poisoned");
                        state.overlay.mode = "demo".to_owned();
                        state.overlay.raids = 12;
                        state.overlay.extractions = 7;
                        state.overlay.eliminations = 19;
                        state.overlay.deaths = 5;
                        state.overlay.loot_value = 428_750;
                        state.overlay.arc_eliminations = 146;
                        state.overlay.downs = 24;
                        state.overlay.revives = 8;
                        state.overlay.containers_looted = 317;
                        state.overlay.items_crafted = 64;
                        state.overlay.damage_by_enemy = 37_420;
                        state.overlay.damage_by_weapon = 91_680;
                        state.overlay.raider_damage = 28_640;
                        state.overlay.duration_ms = 4_327_000;
                        state.overlay.value_brought_in = 184_500;
                        state.overlay.xp_gained = 38_740;
                        state.overlay.stats_rows = 1_587;
                        state.overlay.session_downs = 6;
                        state.overlay.session_extractions = 3;
                        state.overlay.session_deaths = 2;
                        state.overlay.session_loot_value = 128_400;
                        state.overlay.session_money_delta = -56_100;
                        state.overlay.today_extractions = 5;
                        state.overlay.today_deaths = 3;
                        state.overlay.today_available = true;
                        state.overlay.session_raw_totals.insert(
                            "event.200.target.995408715".into(),
                            6,
                        );
                        state
                            .overlay
                            .session_raw_totals
                            .insert("event.101".into(), 55_600);
                        state.overlay.session_raw_totals.insert(
                            "event.101.target.995408715".into(),
                            18_700,
                        );
                        state.overlay.session_raw_totals.insert(
                            "event.101.target.200993951".into(),
                            900,
                        );
                        self.widget_config.apply(&mut state.overlay);
                        state.record("info", "OBS demo statistics loaded");
                        state.clone()
                    };
                    self.server.notify(&updated);
                }
                if snapshot.overlay.mode == "demo" && ui.button("Clear demo").clicked() {
                    let updated = {
                        let mut state = self.state.write().expect("state poisoned");
                        let preset = state.overlay.preset.clone();
                        let language = state.overlay.language.clone();
                        let opacity = state.overlay.opacity;
                        let background_preset = state.overlay.background_preset.clone();
                        let background_color = state.overlay.background_color;
                        let background_blur = state.overlay.background_blur;
                        state.overlay = Default::default();
                        state.overlay.mode = "live".to_owned();
                        state.overlay.preset = preset;
                        state.overlay.language = language;
                        state.overlay.opacity = opacity;
                        state.overlay.background_preset = background_preset;
                        state.overlay.background_color = background_color;
                        state.overlay.background_blur = background_blur;
                        state.record("info", "OBS demo statistics cleared");
                        state.clone()
                    };
                    self.server.notify(&updated);
                }
                if snapshot.overlay.mode == "demo" && ui.button("Demo balance + / -").clicked() {
                    let updated = {
                        let mut state = self.state.write().expect("state poisoned");
                        state.overlay.session_money_delta =
                            if state.overlay.session_money_delta < 0 {
                                72_300
                            } else {
                                -56_100
                            };
                        state.record("info", "OBS demo balance sign changed");
                        state.clone()
                    };
                    self.server.notify(&updated);
                }
                ui.label("Use this to design the OBS scene before a real raid.");
            });
            let mut selected_preset = snapshot.overlay.preset.clone();
            ui.horizontal_wrapped(|ui| {
                ui.label("Widget preset:");
                ui.selectable_value(&mut selected_preset, "pve".to_owned(), "PvE");
                ui.selectable_value(&mut selected_preset, "pvp".to_owned(), "PvP");
                ui.selectable_value(
                    &mut selected_preset,
                    "account".to_owned(),
                    "1 · Account totals",
                );
                ui.selectable_value(
                    &mut selected_preset,
                    "session".to_owned(),
                    "2 · Since app launch",
                );
                ui.selectable_value(
                    &mut selected_preset,
                    "outcome".to_owned(),
                    "3 · Win | Lose",
                );
            });
            if selected_preset != snapshot.overlay.preset {
                self.select_overlay_preset(&selected_preset);
            }
            let mut selected_language = snapshot.overlay.language.clone();
            let mut selected_opacity = snapshot.overlay.opacity;
            let mut selected_background_preset = snapshot.overlay.background_preset.clone();
            let mut selected_background_color = snapshot.overlay.background_color;
            let mut selected_background_blur = snapshot.overlay.background_blur;
            ui.horizontal_wrapped(|ui| {
                ui.label("Язык виджета:");
                ui.selectable_value(&mut selected_language, "ru".to_owned(), "Русский");
                ui.selectable_value(&mut selected_language, "en".to_owned(), "English");
            });
            ui.horizontal_wrapped(|ui| {
                ui.label("Быстрый фон:");
                ui.selectable_value(
                    &mut selected_background_preset,
                    "transparent".to_owned(),
                    "Прозрачный",
                );
                ui.selectable_value(
                    &mut selected_background_preset,
                    "smoke".to_owned(),
                    "Дым",
                );
                ui.selectable_value(
                    &mut selected_background_preset,
                    "glass".to_owned(),
                    "Стекло",
                );
                ui.selectable_value(
                    &mut selected_background_preset,
                    "solid".to_owned(),
                    "Плотный",
                );
                ui.selectable_value(
                    &mut selected_background_preset,
                    "custom".to_owned(),
                    "Свой",
                );
            });
            if selected_background_preset != snapshot.overlay.background_preset {
                match selected_background_preset.as_str() {
                    "transparent" => {
                        selected_background_color = [9, 16, 21];
                        selected_opacity = 0;
                        selected_background_blur = 0;
                    }
                    "glass" => {
                        selected_background_color = [16, 30, 36];
                        selected_opacity = 32;
                        selected_background_blur = 12;
                    }
                    "solid" => {
                        selected_background_color = [8, 12, 15];
                        selected_opacity = 82;
                        selected_background_blur = 0;
                    }
                    "custom" => {}
                    _ => {
                        selected_background_preset = "smoke".to_owned();
                        selected_background_color = [9, 16, 21];
                        selected_opacity = 48;
                        selected_background_blur = 4;
                    }
                }
            }
            let mut manual_background_change = false;
            ui.horizontal_wrapped(|ui| {
                ui.label("Свой фон:");
                manual_background_change |= ui
                    .color_edit_button_srgb(&mut selected_background_color)
                    .changed();
                ui.label("Непрозрачность:");
                manual_background_change |= ui
                    .add(egui::Slider::new(&mut selected_opacity, 0..=100).suffix("%"))
                    .changed();
                ui.label("Размытие:");
                manual_background_change |= ui
                    .add(
                        egui::Slider::new(&mut selected_background_blur, 0..=20)
                            .suffix(" px"),
                    )
                    .changed();
            });
            if manual_background_change {
                selected_background_preset = "custom".to_owned();
            }
            if selected_language != snapshot.overlay.language
                || selected_opacity != snapshot.overlay.opacity
                || selected_background_preset != snapshot.overlay.background_preset
                || selected_background_color != snapshot.overlay.background_color
                || selected_background_blur != snapshot.overlay.background_blur
            {
                let updated = {
                    let mut state = self.state.write().expect("state poisoned");
                    state.overlay.language = selected_language;
                    state.overlay.opacity = selected_opacity;
                    state.overlay.background_preset = selected_background_preset;
                    state.overlay.background_color = selected_background_color;
                    state.overlay.background_blur = selected_background_blur;
                    state.record("info", "OBS widget appearance changed");
                    state.clone()
                };
                self.server.notify(&updated);
            }

            ui.add_space(12.0);
            ui.heading("3. Live player stats");
            ui.label("A read-only request follows the same player-stats contract as the game. Raw response values, captured request context, and credentials stay in memory; only normalized totals are exposed to OBS.");
            let ready = self.collector_ready;
            ui.horizontal(|ui| {
                if !self.live_stats_enabled {
                    if ui
                        .add_enabled(ready && !self.probing_stats, egui::Button::new("Start live OBS sync"))
                        .clicked()
                    {
                        self.live_stats_enabled = true;
                        self.next_stats_probe = Instant::now();
                        self.probe_stats();
                    }
                } else if ui.button("Stop live OBS sync").clicked() {
                    self.live_stats_enabled = false;
                }
                if ui
                    .add_enabled(ready && !self.probing_stats, egui::Button::new("Refresh now"))
                    .clicked()
                {
                    self.probe_stats();
                }
                if self.live_stats_enabled {
                    ui.colored_label(Color32::from_rgb(83, 224, 161), "Auto-refresh: every 15 seconds");
                }
            });
            if self.probing_stats {
                ui.spinner();
                ui.label("Syncing normalized player stats...");
            }
            if snapshot.token_seen && !self.collector_ready {
                ui.label("Waiting for the game's player stats request context...");
            }
            if snapshot.overlay.stats_rows > 0 {
                ui.label(format!(
                    "OBS data: {} raids, {} extracts, {} PvP eliminations, {} loot value ({} source rows)",
                    snapshot.overlay.raids,
                    snapshot.overlay.extractions,
                    snapshot.overlay.eliminations,
                    snapshot.overlay.loot_value,
                    snapshot.overlay.stats_rows,
                ));
            }

            ui.add_space(12.0);
            ui.heading("4. After one raid");
            if ui.button("Export sanitized diagnostics").clicked() {
                match arc_live_diagnostics::export(&self.paths, &snapshot, &self.storage) {
                    Ok(path) => { self.state.write().expect("state poisoned").record("success", format!("Diagnostics exported: {}", path.display())); let _ = open::that(path.parent().unwrap_or(&path)); }
                    Err(error) => self.state.write().expect("state poisoned").record("error", format!("Diagnostics export failed: {error:#}")),
                }
            }

            ui.add_space(12.0);
            ui.heading("Activity");
            egui::ScrollArea::vertical().max_height(240.0).show(ui, |ui| {
                for item in &snapshot.activity {
                    ui.horizontal_wrapped(|ui| {
                        ui.monospace(item.at.format("%H:%M:%S").to_string());
                        ui.label(&item.message);
                    });
                }
            });
        });
    }
}

fn grouped(value: u64) -> String {
    let digits = value.to_string();
    let mut result = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, character) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            result.push(' ');
        }
        result.push(character);
    }
    result
}

fn grouped_signed(value: i64) -> String {
    match value.cmp(&0) {
        std::cmp::Ordering::Greater => format!("+{}", grouped(value.unsigned_abs())),
        std::cmp::Ordering::Less => format!("−{}", grouped(value.unsigned_abs())),
        std::cmp::Ordering::Equal => "0".to_owned(),
    }
}

fn grouped_metric(value: i64) -> String {
    if value < 0 {
        format!("−{}", grouped(value.unsigned_abs()))
    } else {
        grouped(value as u64)
    }
}

impl eframe::App for ArcLiveApp {
    fn ui(&mut self, root: &mut egui::Ui, _frame: &mut eframe::Frame) {
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
        self.maybe_probe_stats();
        root.ctx().request_repaint_after(Duration::from_millis(250));
        let snapshot = self.state.read().expect("state poisoned").clone();
        let ready = self.collector_ready;

        egui::CentralPanel::default().show(root, |ui| {
            ui.horizontal(|ui| {
                ui.heading(RichText::new("ARC Live").size(30.0).strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let (text, color) = if snapshot.game_running && ready {
                        ("● Игра подключена", Color32::from_rgb(83, 224, 161))
                    } else if snapshot.game_running {
                        ("● Подключаем статистику…", Color32::from_rgb(246, 183, 60))
                    } else {
                        ("● Ожидаем игру", Color32::GRAY)
                    };
                    ui.colored_label(color, text);
                });
            });
            ui.label(RichText::new("Статистика ARC Raiders для OBS").color(Color32::GRAY));
            ui.add_space(10.0);

            ui.horizontal(|ui| {
                if ui
                    .selectable_label(self.page == AppPage::Home, "Главная")
                    .clicked()
                {
                    self.page = AppPage::Home;
                }
                if ui
                    .selectable_label(self.page == AppPage::Widget, "Виджет OBS")
                    .clicked()
                {
                    self.page = AppPage::Widget;
                }
                if ui
                    .selectable_label(self.page == AppPage::Settings, "Настройки")
                    .clicked()
                {
                    self.page = AppPage::Settings;
                }
            });
            ui.separator();
            ui.add_space(8.0);

            match self.page {
                AppPage::Home => {
                    egui::Frame::group(ui.style())
                        .inner_margin(egui::Margin::same(18))
                        .show(ui, |ui| {
                            if snapshot.game_running && ready {
                                ui.heading("Всё работает");
                                ui.label("ARC Live подключилась к игре и обновляет статистику автоматически.");
                                if self.probing_stats {
                                    ui.horizontal(|ui| {
                                        ui.spinner();
                                        ui.label("Получаем свежую статистику…");
                                    });
                                } else if snapshot.overlay.stats_rows > 0 {
                                    ui.colored_label(
                                        Color32::from_rgb(83, 224, 161),
                                        "Данные для OBS актуальны",
                                    );
                                }
                            } else if snapshot.game_running {
                                ui.heading("Подключаемся к игре…");
                                ui.label("Ничего делать не нужно. Обычно это занимает несколько секунд.");
                                ui.spinner();
                            } else {
                                ui.heading("Можно запускать ARC Raiders");
                                ui.label("Запусти Steam или Epic и игру как обычно. ARC Live подключится сама — ничего перезапускать не потребуется.");
                            }
                        });

                    ui.add_space(14.0);
                    ui.heading("Текущий стрим");
                    let mut quick_preset = snapshot.overlay.preset.clone();
                    ui.horizontal_wrapped(|ui| {
                        ui.label("Режим в OBS:");
                        ui.selectable_value(
                            &mut quick_preset,
                            "pve".to_owned(),
                            RichText::new("PvE · лут и ARC").size(16.0).strong(),
                        );
                        ui.selectable_value(
                            &mut quick_preset,
                            "pvp".to_owned(),
                            RichText::new("PvP · ноки и урон").size(16.0).strong(),
                        );
                        ui.separator();
                        let can_reset = snapshot.overlay.stats_rows > 0
                            && snapshot.overlay.mode != "demo";
                        if ui
                            .add_enabled(
                                can_reset,
                                egui::Button::new("Сбросить статистику стрима"),
                            )
                            .clicked()
                        {
                            self.confirm_new_stream = true;
                        }
                    });
                    if quick_preset != snapshot.overlay.preset {
                        self.select_overlay_preset(&quick_preset);
                    }
                    ui.horizontal_wrapped(|ui| {
                        let started = self
                            .stream_started_at
                            .map(|at| {
                                at.with_timezone(&Local)
                                    .format("сегодня с %H:%M")
                                    .to_string()
                            })
                            .unwrap_or_else(|| "начнётся после первой синхронизации".to_owned());
                        ui.label(started);
                    });
                    ui.label(
                        RichText::new(
                            "Если игра или ARC Live закроются, эти счётчики восстановятся автоматически.",
                        )
                        .color(Color32::GRAY),
                    );
                    if self.confirm_new_stream {
                        egui::Frame::group(ui.style())
                            .inner_margin(egui::Margin::same(12))
                            .show(ui, |ui| {
                                ui.label("Обнулить PvE- и PvP-статистику текущего стрима?");
                                ui.horizontal(|ui| {
                                    if ui.button("Да, сбросить").clicked() {
                                        self.start_new_stream();
                                    }
                                    if ui.button("Отмена").clicked() {
                                        self.confirm_new_stream = false;
                                    }
                                });
                            });
                    }

                    ui.add_space(14.0);
                    egui::CollapsingHeader::new("События за сегодня")
                        .default_open(true)
                        .show(ui, |ui| {
                            if self.user_events.is_empty() {
                                ui.label(RichText::new("Событий пока нет").color(Color32::GRAY));
                            }
                            for event in self.user_events.iter().take(6) {
                                let color = match event.level.as_str() {
                                    "success" => Color32::from_rgb(83, 224, 161),
                                    "warning" | "error" => Color32::from_rgb(246, 183, 60),
                                    _ => Color32::GRAY,
                                };
                                ui.horizontal_wrapped(|ui| {
                                    ui.colored_label(
                                        color,
                                        event
                                            .at
                                            .with_timezone(&Local)
                                            .format("%H:%M")
                                            .to_string(),
                                    );
                                    ui.label(&event.message);
                                });
                            }
                        });

                    ui.add_space(14.0);
                    ui.heading("OBS");
                    let overlay_url = format!("{}/overlay/live", snapshot.local_url);
                    ui.label("Добавь этот адрес в OBS как Browser Source один раз:");
                    ui.horizontal_wrapped(|ui| {
                        ui.monospace(&overlay_url);
                        if ui.button("Скопировать ссылку").clicked() {
                            ui.ctx().copy_text(overlay_url.clone());
                        }
                        if ui.button("Открыть превью").clicked() {
                            let _ = open::that(&overlay_url);
                        }
                    });
                    ui.label(RichText::new("Рекомендуемый размер: 700 × 80").color(Color32::GRAY));
                    if ui.button("Настроить виджет").clicked() {
                        self.page = AppPage::Widget;
                    }

                    ui.add_space(18.0);
                    ui.collapsing("Если что-то не работает", |ui| {
                        ui.label("Сохрани безопасный диагностический архив и передай его поддержке.");
                        if ui.button("Сохранить диагностику").clicked() {
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
                                    .record(
                                        "error",
                                        format!("Diagnostics export failed: {error:#}"),
                                    ),
                            }
                        }
                    });
                }
                AppPage::Widget => {
                    ui.heading("Живое превью");
                    let overlay = &snapshot.overlay;
                    let russian = overlay.language != "en";
                    let mut preview_metrics: Vec<(String, String, Color32)> = Vec::new();
                    match overlay.preset.as_str() {
                        "pve" => {
                            preview_metrics.push((
                                grouped_metric(overlay.widget_pve[0]),
                                if russian {
                                    overlay.widget_pve_labels_ru[0].clone()
                                } else {
                                    overlay.widget_pve_labels_en[0].clone()
                                },
                                Color32::from_rgb(246, 183, 60),
                            ));
                            preview_metrics.push((
                                grouped_metric(overlay.widget_pve[1]),
                                if russian {
                                    overlay.widget_pve_labels_ru[1].clone()
                                } else {
                                    overlay.widget_pve_labels_en[1].clone()
                                },
                                Color32::from_rgb(83, 224, 161),
                            ));
                        }
                        "pvp" => {
                            preview_metrics.push((
                                grouped_metric(overlay.widget_pvp[0]),
                                if russian {
                                    overlay.widget_pvp_labels_ru[0].clone()
                                } else {
                                    overlay.widget_pvp_labels_en[0].clone()
                                },
                                Color32::WHITE,
                            ));
                            preview_metrics.push((
                                grouped_metric(overlay.widget_pvp[1]),
                                if russian {
                                    overlay.widget_pvp_labels_ru[1].clone()
                                } else {
                                    overlay.widget_pvp_labels_en[1].clone()
                                },
                                Color32::from_rgb(255, 112, 124),
                            ));
                        }
                        "session" => {
                            preview_metrics.push((
                                grouped_metric(overlay.widget_session[0]),
                                if russian {
                                    overlay.widget_session_labels_ru[0].clone()
                                } else {
                                    overlay.widget_session_labels_en[0].clone()
                                },
                                Color32::WHITE,
                            ));
                            preview_metrics.push((
                                grouped_metric(overlay.widget_session[1]),
                                if russian {
                                    overlay.widget_session_labels_ru[1].clone()
                                } else {
                                    overlay.widget_session_labels_en[1].clone()
                                },
                                Color32::from_rgb(83, 224, 161),
                            ));
                            preview_metrics.push((
                                grouped_signed(overlay.widget_session[2]),
                                if russian {
                                    overlay.widget_session_labels_ru[2].clone()
                                } else {
                                    overlay.widget_session_labels_en[2].clone()
                                },
                                if overlay.widget_session[2] < 0 {
                                    Color32::from_rgb(255, 112, 124)
                                } else {
                                    Color32::from_rgb(83, 224, 161)
                                },
                            ));
                        }
                        "outcome" => {
                            preview_metrics.push((
                                grouped_metric(overlay.widget_outcome[0]),
                                if russian {
                                    overlay.widget_outcome_labels_ru[0].clone()
                                } else {
                                    overlay.widget_outcome_labels_en[0].clone()
                                },
                                Color32::from_rgb(83, 224, 161),
                            ));
                            preview_metrics.push((
                                grouped_metric(overlay.widget_outcome[1]),
                                if russian {
                                    overlay.widget_outcome_labels_ru[1].clone()
                                } else {
                                    overlay.widget_outcome_labels_en[1].clone()
                                },
                                Color32::from_rgb(255, 112, 124),
                            ));
                        }
                        _ => {
                            preview_metrics.push((
                                grouped_metric(overlay.widget_account[0]),
                                if russian {
                                    overlay.widget_account_labels_ru[0].clone()
                                } else {
                                    overlay.widget_account_labels_en[0].clone()
                                },
                                Color32::WHITE,
                            ));
                            preview_metrics.push((
                                grouped_metric(overlay.widget_account[1]),
                                if russian {
                                    overlay.widget_account_labels_ru[1].clone()
                                } else {
                                    overlay.widget_account_labels_en[1].clone()
                                },
                                Color32::from_rgb(83, 224, 161),
                            ));
                            preview_metrics.push((
                                grouped_metric(overlay.widget_account[2]),
                                if russian {
                                    overlay.widget_account_labels_ru[2].clone()
                                } else {
                                    overlay.widget_account_labels_en[2].clone()
                                },
                                Color32::from_rgb(246, 183, 60),
                            ));
                        }
                    }

                    let [red, green, blue] = overlay.background_color;
                    let preview_background = Color32::from_rgba_unmultiplied(
                        red,
                        green,
                        blue,
                        ((u16::from(overlay.opacity) * 255) / 100) as u8,
                    );
                    egui::Frame::new()
                        .fill(preview_background)
                        .stroke(egui::Stroke::new(1.0, Color32::from_white_alpha(45)))
                        .corner_radius(egui::CornerRadius::same(8))
                        .inner_margin(egui::Margin::same(10))
                        .show(ui, |ui| {
                            let preview_top = ui.cursor().top();
                            ui.columns(preview_metrics.len(), |columns| {
                                for (column, (value, label, color)) in
                                    columns.iter_mut().zip(preview_metrics.iter())
                                {
                                    column.label(RichText::new(value).size(29.0).strong().color(*color));
                                    column.label(RichText::new(label).size(11.0).color(Color32::LIGHT_GRAY));
                                }
                            });
                            if preview_metrics.len() == 2 {
                                let rect = ui.min_rect();
                                ui.painter().line_segment(
                                    [
                                        egui::pos2(rect.center().x, preview_top),
                                        egui::pos2(rect.center().x, rect.bottom()),
                                    ],
                                    egui::Stroke::new(1.0, Color32::from_white_alpha(45)),
                                );
                            }
                        });

                    ui.add_space(14.0);
                    ui.heading("Что показывать");
                    let mut edited = overlay.clone();
                    ui.horizontal_wrapped(|ui| {
                        ui.selectable_value(&mut edited.preset, "pve".to_owned(), "PvE");
                        ui.selectable_value(&mut edited.preset, "pvp".to_owned(), "PvP");
                        ui.separator();
                        ui.selectable_value(&mut edited.preset, "account".to_owned(), "Аккаунт");
                        ui.selectable_value(&mut edited.preset, "session".to_owned(), "Весь стрим");
                        ui.selectable_value(&mut edited.preset, "outcome".to_owned(), "Win | Lose");
                        ui.separator();
                        ui.selectable_value(&mut edited.language, "ru".to_owned(), "Русский");
                        ui.selectable_value(&mut edited.language, "en".to_owned(), "English");
                    });

                    ui.add_space(12.0);
                    ui.heading("Фон");
                    ui.horizontal_wrapped(|ui| {
                        ui.label("Быстрый вариант:");
                        ui.selectable_value(
                            &mut edited.background_preset,
                            "transparent".to_owned(),
                            "Прозрачный",
                        );
                        ui.selectable_value(
                            &mut edited.background_preset,
                            "smoke".to_owned(),
                            "Дым",
                        );
                        ui.selectable_value(
                            &mut edited.background_preset,
                            "glass".to_owned(),
                            "Стекло",
                        );
                        ui.selectable_value(
                            &mut edited.background_preset,
                            "solid".to_owned(),
                            "Плотный",
                        );
                    });
                    if edited.background_preset != overlay.background_preset {
                        match edited.background_preset.as_str() {
                            "transparent" => {
                                edited.background_color = [9, 16, 21];
                                edited.opacity = 0;
                                edited.background_blur = 0;
                            }
                            "glass" => {
                                edited.background_color = [16, 30, 36];
                                edited.opacity = 32;
                                edited.background_blur = 12;
                            }
                            "solid" => {
                                edited.background_color = [8, 12, 15];
                                edited.opacity = 82;
                                edited.background_blur = 0;
                            }
                            _ => {
                                edited.background_preset = "smoke".to_owned();
                                edited.background_color = [9, 16, 21];
                                edited.opacity = 48;
                                edited.background_blur = 4;
                            }
                        }
                    }
                    let mut manual_background_change = false;
                    ui.horizontal_wrapped(|ui| {
                        ui.label("Свой фон:");
                        manual_background_change |= ui
                            .color_edit_button_srgb(&mut edited.background_color)
                            .changed();
                        ui.label("Непрозрачность:");
                        manual_background_change |= ui
                            .add(egui::Slider::new(&mut edited.opacity, 0..=100).suffix("%"))
                            .changed();
                        ui.label("Размытие:");
                        manual_background_change |= ui
                            .add(
                                egui::Slider::new(&mut edited.background_blur, 0..=20)
                                    .suffix(" px"),
                            )
                            .changed();
                    });
                    if manual_background_change {
                        edited.background_preset = "custom".to_owned();
                    }

                    let appearance_changed = edited.preset != overlay.preset
                        || edited.language != overlay.language
                        || edited.background_preset != overlay.background_preset
                        || edited.background_color != overlay.background_color
                        || edited.opacity != overlay.opacity
                        || edited.background_blur != overlay.background_blur;
                    if appearance_changed {
                        let updated = {
                            let mut state = self.state.write().expect("state poisoned");
                            state.overlay.preset = edited.preset.clone();
                            state.overlay.language = edited.language.clone();
                            state.overlay.background_preset = edited.background_preset.clone();
                            state.overlay.background_color = edited.background_color;
                            state.overlay.opacity = edited.opacity;
                            state.overlay.background_blur = edited.background_blur;
                            state.record("info", "Widget settings changed");
                            state.clone()
                        };
                        self.server.notify(&updated);
                        self.save_overlay_preferences(&edited);
                    }

                    ui.add_space(12.0);
                    ui.heading("Тестовые данные");
                    ui.horizontal_wrapped(|ui| {
                        if snapshot.overlay.mode != "demo" {
                            if ui.button("Показать тестовые данные").clicked() {
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
                                    state.overlay.raw_totals.insert(
                                        "event.200.target.995408715".into(),
                                        24,
                                    );
                                    state
                                        .overlay
                                        .raw_totals
                                        .insert("event.101".into(), 28_640);
                                    state.overlay.session_raw_totals.insert(
                                        "event.200.target.995408715".into(),
                                        6,
                                    );
                                    state
                                        .overlay
                                        .session_raw_totals
                                        .insert("event.101".into(), 55_600);
                                    state.overlay.session_raw_totals.insert(
                                        "event.101.target.995408715".into(),
                                        18_700,
                                    );
                                    state.overlay.session_raw_totals.insert(
                                        "event.101.target.200993951".into(),
                                        900,
                                    );
                                    self.widget_config.apply(&mut state.overlay);
                                    state.record("info", "OBS demo statistics loaded");
                                    state.clone()
                                };
                                self.server.notify(&updated);
                            }
                        } else {
                            if ui.button("Вернуться к реальным данным").clicked() {
                                let updated = {
                                    let mut state = self.state.write().expect("state poisoned");
                                    let preferences = (
                                        state.overlay.preset.clone(),
                                        state.overlay.language.clone(),
                                        state.overlay.background_preset.clone(),
                                        state.overlay.background_color,
                                        state.overlay.opacity,
                                        state.overlay.background_blur,
                                    );
                                    state.overlay = Default::default();
                                    state.overlay.mode = "live".to_owned();
                                    state.overlay.preset = preferences.0;
                                    state.overlay.language = preferences.1;
                                    state.overlay.background_preset = preferences.2;
                                    state.overlay.background_color = preferences.3;
                                    state.overlay.opacity = preferences.4;
                                    state.overlay.background_blur = preferences.5;
                                    self.widget_config.apply(&mut state.overlay);
                                    state.record("info", "OBS demo statistics cleared");
                                    state.clone()
                                };
                                self.server.notify(&updated);
                                self.next_stats_probe = Instant::now();
                            }
                            if ui.button("Баланс + / −").clicked() {
                                let updated = {
                                    let mut state = self.state.write().expect("state poisoned");
                                    state.overlay.session_money_delta =
                                        if state.overlay.session_money_delta < 0 {
                                            72_300
                                        } else {
                                            -56_100
                                        };
                                    state.record("info", "OBS demo balance sign changed");
                                    state.clone()
                                };
                                self.server.notify(&updated);
                            }
                        }
                    });

                    ui.add_space(12.0);
                    let overlay_url = format!("{}/overlay/live", snapshot.local_url);
                    ui.horizontal_wrapped(|ui| {
                        if ui.button("Скопировать ссылку OBS").clicked() {
                            ui.ctx().copy_text(overlay_url.clone());
                        }
                        if ui.button("Открыть настоящее превью").clicked() {
                            let _ = open::that(&overlay_url);
                        }
                        ui.label(RichText::new("Изменения сохраняются автоматически").color(Color32::GRAY));
                    });
                    ui.add_space(8.0);
                    ui.collapsing("Расширенная настройка данных", |ui| {
                        ui.label("Источники чисел и подписи задаются в пользовательском файле конфигурации.");
                        if ui.button("Открыть widget-config.json").clicked() {
                            let _ = open::that(&self.paths.widget_config);
                        }
                        ui.label(
                            RichText::new("Исправления применяются при следующей синхронизации — новый EXE не нужен.")
                                .color(Color32::GRAY),
                        );
                    });
                }
                AppPage::Settings => {
                    ui.heading("Настройки");
                    ui.add_space(10.0);

                    egui::Frame::group(ui.style())
                        .inner_margin(egui::Margin::same(16))
                        .show(ui, |ui| {
                            ui.heading("Обновления");
                            let mut automatic = self.config.automatic_updates;
                            if ui
                                .checkbox(&mut automatic, "Автоматически проверять обновления")
                                .changed()
                            {
                                self.config.automatic_updates = automatic;
                                let _ = self.config.save(&self.paths.config);
                            }
                            ui.horizontal_wrapped(|ui| {
                                ui.label("Канал:");
                                let mut channel = self.config.update_channel.clone();
                                ui.selectable_value(&mut channel, "stable".to_owned(), "Стабильный");
                                ui.selectable_value(&mut channel, "beta".to_owned(), "Бета");
                                if channel != self.config.update_channel {
                                    self.config.update_channel = channel;
                                    self.updates.available = None;
                                    let _ = self.config.save(&self.paths.config);
                                }
                            });
                            ui.horizontal_wrapped(|ui| {
                                let selected_feed = self.config.selected_update_feed_url();
                                if ui
                                    .add_enabled(
                                        !self.updates.checking && !selected_feed.is_empty(),
                                        egui::Button::new("Проверить сейчас"),
                                    )
                                    .clicked()
                                {
                                    self.updates.check(
                                        selected_feed.clone(),
                                        self.config.update_channel.clone(),
                                    );
                                }
                                if self.updates.checking {
                                    ui.spinner();
                                    ui.label("Проверяем…");
                                } else if let Some(manifest) = self.updates.available.clone() {
                                    ui.colored_label(
                                        Color32::from_rgb(83, 224, 161),
                                        format!("Доступна версия {}", manifest.version),
                                    );
                                    if self.updates.downloaded.is_none()
                                        && ui
                                            .add_enabled(
                                                !self.updates.downloading,
                                                egui::Button::new("Скачать"),
                                            )
                                            .clicked()
                                    {
                                        self.updates.download(self.paths.updates.clone());
                                    }
                                } else if selected_feed.is_empty() {
                                    ui.label(
                                        RichText::new("Канал обновлений будет активирован в публичной сборке")
                                            .color(Color32::GRAY),
                                    );
                                } else {
                                    ui.label("Установлена актуальная версия");
                                }
                            });
                            if self.updates.downloading {
                                ui.horizontal(|ui| {
                                    ui.spinner();
                                    ui.label("Скачиваем и проверяем обновление…");
                                });
                            }
                            if self.updates.downloaded.is_some() {
                                ui.colored_label(
                                    Color32::from_rgb(83, 224, 161),
                                    "Обновление скачано и проверено",
                                );
                                if snapshot.game_running {
                                    ui.label("Установка станет доступна после закрытия игры.");
                                } else if ui.button("Установить и закрыть ARC Live").clicked() {
                                    match self.updates.launch_downloaded_after_exit() {
                                        Ok(()) => {
                                            ui.ctx().send_viewport_cmd(
                                                egui::ViewportCommand::Close,
                                            );
                                        }
                                        Err(error) => {
                                            self.updates.error = Some(format!("{error:#}"));
                                        }
                                    }
                                }
                            }
                            if let Some(error) = &self.updates.error {
                                ui.colored_label(Color32::from_rgb(255, 112, 124), error);
                            }
                        });

                    ui.add_space(12.0);
                    egui::Frame::group(ui.style())
                        .inner_margin(egui::Margin::same(16))
                        .show(ui, |ui| {
                            ui.heading("Файлы и восстановление");
                            ui.label("История стрима и настройки хранятся отдельно от программы и переживают обновления.");
                            ui.horizontal_wrapped(|ui| {
                                if ui.button("Открыть папку данных").clicked() {
                                    let _ = open::that(&self.paths.root);
                                }
                                if ui.button("Открыть конфигурацию виджета").clicked() {
                                    let _ = open::that(&self.paths.widget_config);
                                }
                            });
                        });

                    ui.add_space(12.0);
                    ui.label(
                        RichText::new(format!(
                            "ARC Live {} · фоновый компонент: {}",
                            env!("CARGO_PKG_VERSION"),
                            if self.collector_privileged {
                                "работает"
                            } else {
                                "portable-режим"
                            }
                        ))
                        .color(Color32::GRAY),
                    );
                }
            }
        });
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
