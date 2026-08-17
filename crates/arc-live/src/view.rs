//! Pure rendering layer for the ARC Live window.
//!
//! It takes a read-only [`ViewModel`] and returns the [`Action`]s the user
//! asked for, so the same code renders the real app and the offline
//! `ui_preview` example used for design screenshots.

use arc_live_core::state::{GameKeylogStatus, OverlayCell, OverlayPreset, OverlayStats};
use eframe::egui::{self, Color32, RichText};

pub const COLOR_ACCENT: Color32 = Color32::from_rgb(83, 224, 161);
pub const COLOR_DANGER: Color32 = Color32::from_rgb(255, 112, 124);
pub const COLOR_LOOT: Color32 = Color32::from_rgb(246, 183, 60);
pub const COLOR_MUTED: Color32 = Color32::from_rgb(150, 160, 168);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Stream,
    Widget,
    Settings,
}

#[derive(Debug, Clone)]
pub struct EventView {
    pub time: String,
    pub level: String,
    pub message: String,
}

#[derive(Debug, Clone, Copy)]
pub struct ConnectionView {
    pub game_running: bool,
    pub stats_ready: bool,
    pub launcher_prepared: bool,
    pub service_privileged: bool,
}

/// Raw capture evidence, used to explain why nothing arrives.
#[derive(Debug, Clone, Copy, Default)]
pub struct CaptureView {
    pub handshakes: u64,
    pub key_errors: u64,
    pub decrypted: u64,
    pub game_keylog: GameKeylogStatus,
}

/// Why the statistics are not arriving, in the order the user should act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureProblem {
    /// The game runs without `SSLKEYLOGFILE` - its launcher predates the setup.
    GameWithoutKeys,
    /// The game writes keys to a file ARC Live does not read.
    GameKeysElsewhere,
    /// Handshakes are seen but no key matches them.
    KeysDoNotMatch,
}

/// Decides what to tell the user. Pure so the rule stays testable.
pub fn capture_problem(
    connection: &ConnectionView,
    capture: &CaptureView,
) -> Option<CaptureProblem> {
    if !connection.game_running || capture.decrypted > 0 {
        return None;
    }
    match capture.game_keylog {
        GameKeylogStatus::Missing => Some(CaptureProblem::GameWithoutKeys),
        GameKeylogStatus::Different => Some(CaptureProblem::GameKeysElsewhere),
        _ if capture.handshakes > 0 && capture.key_errors > 0 => {
            Some(CaptureProblem::KeysDoNotMatch)
        }
        _ => None,
    }
}

/// The risk notice, shown before first use and repeated in "О программе".
/// Кратко и честно: метод работы приложения прямо запрещён соглашением игры.
pub const DISCLAIMER_TITLE: &str = "Прочитайте до начала работы";
pub const DISCLAIMER_PARAGRAPHS: [&str; 4] = [
    "ARC Live - независимый любительский проект. Он не связан с Embark Studios, \
     не одобрен и не поддерживается ими. ARC Raiders и все материалы игры принадлежат \
     Embark Studios.",
    "Это исследовательская сборка. Чтобы посчитать статистику, ARC Live расшифровывает \
     и читает сетевые ответы игры на вашем компьютере. Пользовательское соглашение игры \
     запрещает перехват и анализ её сетевого протокола, а античит может счесть нарушением \
     любую стороннюю программу рядом с игрой.",
    "Из этого следует главное: использование ARC Live может привести к блокировке аккаунта - \
     временной или постоянной, вплоть до потери всего купленного и пройденного. \
     Гарантий, что этого не произойдёт, никто дать не может.",
    "Программа поставляется как есть. Автор не несёт ответственности за блокировку аккаунта, \
     потерю прогресса и любой другой ущерб. Вы используете её на свой страх и риск. \
     Если аккаунт вам дорог - не используйте ARC Live на нём.",
];
/// What the application deliberately does not do. Kept next to the warning so
/// the picture is complete rather than one-sided.
pub const DISCLAIMER_LIMITS: [&str; 4] = [
    "не изменяет файлы игры и не читает её память",
    "не управляет вводом и не даёт преимущества в бою",
    "не отправляет наружу ни статистику, ни ключи, ни токены",
    "не обращается к серверам Embark от своего имени",
];
pub const DISCLAIMER_CONFIRMATION: &str = "Я прочитал, понимаю риск блокировки и принимаю его";

/// Draws the risk notice that gates the first run. Returns the actions the
/// user asked for; the caller owns both the checkbox state and the decision.
pub fn disclaimer(ui: &mut egui::Ui, confirmed: bool) -> Vec<Action> {
    let mut actions = Vec::new();
    egui::CentralPanel::default()
        .frame(panel_frame(ui, 22, 18))
        .show(ui, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.label(
                        RichText::new(DISCLAIMER_TITLE)
                            .size(26.0)
                            .strong()
                            .color(COLOR_LOOT),
                    );
                    ui.add_space(14.0);
                    for paragraph in DISCLAIMER_PARAGRAPHS {
                        ui.label(RichText::new(paragraph).size(15.0));
                        ui.add_space(10.0);
                    }
                    ui.add_space(4.0);
                    card(ui, |ui| {
                        ui.label(RichText::new("Что ARC Live не делает:").strong());
                        ui.add_space(4.0);
                        for limit in DISCLAIMER_LIMITS {
                            ui.label(RichText::new(format!("• {limit}")).color(COLOR_MUTED));
                        }
                    });
                    ui.add_space(16.0);
                    let mut checked = confirmed;
                    if ui.checkbox(&mut checked, DISCLAIMER_CONFIRMATION).changed() {
                        actions.push(Action::ConfirmDisclaimer(checked));
                    }
                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        if ui
                            .add_enabled(confirmed, egui::Button::new("Продолжить"))
                            .clicked()
                        {
                            actions.push(Action::AcceptDisclaimer);
                        }
                        if ui.button("Закрыть программу").clicked() {
                            actions.push(Action::DeclineDisclaimer);
                        }
                    });
                });
        });
    actions
}

#[derive(Debug, Clone)]
pub struct StreamView {
    pub started_at: Option<String>,
    pub can_reset: bool,
    pub confirm_reset: bool,
}

#[derive(Debug, Clone, Default)]
pub struct UpdateView {
    pub automatic: bool,
    pub channel: String,
    pub checking: bool,
    pub downloading: bool,
    pub available: Option<String>,
    pub downloaded: bool,
    pub blocked_by_game: bool,
    pub error: Option<String>,
}

pub struct ViewModel<'a> {
    pub page: Page,
    pub version: &'a str,
    /// Packet source in use, shown in "О программе".
    pub capture_backend: &'a str,
    pub overlay: &'a OverlayStats,
    pub connection: ConnectionView,
    pub stream: StreamView,
    pub events: &'a [EventView],
    pub obs_url: &'a str,
    pub updates: UpdateView,
    pub capture: CaptureView,
    /// Launchers ARC Live can restart with the key variable already set.
    pub launchers: &'a [String],
    pub preset_error: Option<&'a str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Goto(Page),
    SelectPreset(String),
    AskReset,
    CancelReset,
    ConfirmReset,
    CopyObsUrl,
    OpenObsPreview,
    OpenPresetFile,
    ReloadPresets,
    /// Reread config.json and widget-config.json without restarting.
    ReloadConfig,
    OpenDataFolder,
    ExportDiagnostics,
    SetLanguage(String),
    SetAppearance {
        preset: String,
        color: [u8; 3],
        opacity: u8,
        blur: u8,
    },
    SetDemo(bool),
    FlipDemoBalance,
    SetAutoUpdates(bool),
    SetChannel(String),
    CheckUpdates,
    DownloadUpdate,
    InstallUpdate,
    /// Close the launcher and start it again with SSLKEYLOGFILE in place.
    RestartLauncher(usize),
    /// The risk-notice checkbox was toggled.
    ConfirmDisclaimer(bool),
    /// The risk notice was accepted and must not be shown again.
    AcceptDisclaimer,
    /// The risk notice was refused, so the application closes.
    DeclineDisclaimer,
}

/// Installs the ARC Live look. Called once by the app and by the preview.
pub fn install_theme(ctx: &egui::Context) {
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = Color32::from_rgb(17, 21, 25);
    visuals.window_fill = Color32::from_rgb(21, 26, 31);
    visuals.extreme_bg_color = Color32::from_rgb(12, 15, 18);
    visuals.widgets.noninteractive.bg_fill = Color32::from_rgb(24, 29, 34);
    visuals.widgets.inactive.bg_fill = Color32::from_rgb(33, 40, 46);
    visuals.selection.bg_fill = Color32::from_rgb(38, 86, 74);
    ctx.set_visuals(visuals);
    ctx.all_styles_mut(|style| {
        style.spacing.item_spacing = egui::vec2(8.0, 6.0);
        style.spacing.button_padding = egui::vec2(10.0, 6.0);
    });
}

/// Draws a status dot. The bundled font has no filled-circle glyph, so it is
/// painted instead of written.
fn dot(ui: &mut egui::Ui, color: Color32, filled: bool) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(13.0, 13.0), egui::Sense::hover());
    if filled {
        ui.painter().circle_filled(rect.center(), 5.0, color);
    } else {
        ui.painter()
            .circle_stroke(rect.center(), 4.5, egui::Stroke::new(1.5, color));
    }
}

/// Renders one preset value the way the OBS widget renders it.
pub fn cell_text(ui: &egui::Ui, cell: &OverlayCell) -> (String, Color32) {
    match cell.style.as_str() {
        "balance" => (
            grouped_signed(cell.value),
            if cell.value < 0 {
                COLOR_DANGER
            } else {
                COLOR_ACCENT
            },
        ),
        "accent" => (grouped_metric(cell.value), COLOR_ACCENT),
        "danger" => (grouped_metric(cell.value), COLOR_DANGER),
        "loot" => (grouped_metric(cell.value), COLOR_LOOT),
        _ => (grouped_metric(cell.value), ui.visuals().strong_text_color()),
    }
}

pub fn grouped(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, character) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push('\u{2009}');
        }
        out.push(character);
    }
    out
}

pub fn grouped_signed(value: i64) -> String {
    match value.cmp(&0) {
        std::cmp::Ordering::Greater => format!("+{}", grouped(value.unsigned_abs())),
        std::cmp::Ordering::Less => format!("\u{2212}{}", grouped(value.unsigned_abs())),
        std::cmp::Ordering::Equal => "0".to_owned(),
    }
}

pub fn grouped_metric(value: i64) -> String {
    if value < 0 {
        grouped_signed(value)
    } else {
        grouped(value.unsigned_abs())
    }
}

pub fn render(ui: &mut egui::Ui, model: &ViewModel<'_>) -> Vec<Action> {
    let mut actions = Vec::new();
    egui::Panel::top("header")
        .frame(panel_frame(ui, 16, 12))
        .show(ui, |ui| header(ui, model, &mut actions));
    egui::Panel::bottom("obs-bar")
        .frame(panel_frame(ui, 16, 10))
        .show(ui, |ui| obs_bar(ui, model, &mut actions));
    egui::CentralPanel::default()
        .frame(panel_frame(ui, 16, 14))
        .show(ui, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| match model.page {
                    Page::Stream => stream_page(ui, model, &mut actions),
                    Page::Widget => widget_page(ui, model, &mut actions),
                    Page::Settings => settings_page(ui, model, &mut actions),
                });
        });
    actions
}

fn panel_frame(ui: &egui::Ui, horizontal: i8, vertical: i8) -> egui::Frame {
    egui::Frame::new()
        .fill(ui.style().visuals.panel_fill)
        .inner_margin(egui::Margin {
            left: horizontal,
            right: horizontal,
            top: vertical,
            bottom: vertical,
        })
}

fn header(ui: &mut egui::Ui, model: &ViewModel<'_>, actions: &mut Vec<Action>) {
    ui.horizontal(|ui| {
        ui.heading(RichText::new("ARC Live").size(26.0).strong());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let (text, color) = connection_badge(&model.connection);
            ui.label(RichText::new(text).color(color).strong());
            dot(ui, color, true);
        });
    });
    ui.add_space(10.0);
    ui.horizontal(|ui| {
        for (page, title) in [
            (Page::Stream, "Стрим"),
            (Page::Widget, "Виджет OBS"),
            (Page::Settings, "Настройки"),
        ] {
            let selected = model.page == page;
            if ui
                .selectable_label(selected, RichText::new(title).size(15.0))
                .clicked()
                && !selected
            {
                actions.push(Action::Goto(page));
            }
        }
    });
}

fn connection_badge(connection: &ConnectionView) -> (&'static str, Color32) {
    if connection.game_running && connection.stats_ready {
        ("Игра подключена", COLOR_ACCENT)
    } else if connection.game_running {
        ("Подключаем статистику…", COLOR_LOOT)
    } else if connection.launcher_prepared {
        ("Ждём запуск игры", COLOR_MUTED)
    } else {
        ("Нужен запуск лаунчера", COLOR_LOOT)
    }
}

/// Always-visible strip: what the viewer sees right now and a one-click switch.
fn obs_bar(ui: &mut egui::Ui, model: &ViewModel<'_>, actions: &mut Vec<Action>) {
    let language = model.overlay.language.clone();
    let active = model.overlay.active_preset();
    ui.horizontal(|ui| {
        ui.label(RichText::new("В OBS сейчас").color(COLOR_MUTED));
        let selected = active.map_or("—", |preset| preset.name(&language));
        egui::ComboBox::from_id_salt("obs-preset")
            .selected_text(RichText::new(selected).strong())
            .width(230.0)
            .show_ui(ui, |ui| {
                for preset in &model.overlay.presets {
                    let chosen =
                        Some(preset.id.as_str()) == active.map(|current| current.id.as_str());
                    if ui
                        .selectable_label(chosen, preset.name(&language))
                        .clicked()
                        && !chosen
                    {
                        actions.push(Action::SelectPreset(preset.id.clone()));
                    }
                }
            });
        if let Some(preset) = active {
            ui.add_space(6.0);
            for cell in preset.cells.iter().take(3) {
                let (value, color) = cell_text(ui, cell);
                ui.label(RichText::new(value).strong().color(color));
                ui.label(
                    RichText::new(cell.label(&language))
                        .small()
                        .color(COLOR_MUTED),
                );
                ui.add_space(8.0);
            }
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if model.overlay.mode == "demo" {
                ui.label(RichText::new("ТЕСТОВЫЕ ДАННЫЕ").small().color(COLOR_LOOT));
            } else {
                ui.label(RichText::new("эфир").small().color(COLOR_MUTED));
            }
        });
    });
}

fn card<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    egui::Frame::group(ui.style())
        .inner_margin(egui::Margin::same(16))
        .corner_radius(egui::CornerRadius::same(10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

fn section(ui: &mut egui::Ui, title: &str) {
    ui.add_space(16.0);
    ui.label(RichText::new(title).size(17.0).strong());
    ui.add_space(8.0);
}

fn stream_page(ui: &mut egui::Ui, model: &ViewModel<'_>, actions: &mut Vec<Action>) {
    // A capture problem replaces the status card: telling the user that
    // everything works right above the reason it does not would be a lie.
    if let Some(problem) = capture_problem(&model.connection, &model.capture) {
        capture_problem_card(ui, problem, &model.capture, model.launchers, actions);
    } else {
        card(ui, |ui| {
            let connection = &model.connection;
            if connection.game_running && connection.stats_ready {
                ui.label(RichText::new("Всё работает").size(19.0).strong());
                ui.label("Статистика обновляется сама при каждом возвращении в Сперанцу.");
            } else if connection.game_running {
                ui.label(RichText::new("Подключаемся к игре…").size(19.0).strong());
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Обычно это занимает несколько секунд. Делать ничего не нужно.");
                });
            } else {
                ui.label(
                    RichText::new("Можно запускать ARC Raiders")
                        .size(19.0)
                        .strong(),
                );
                ui.label("Запусти Steam или Epic и игру как обычно — ARC Live подключится сама.");
            }
        });
    }

    section(ui, "Текущий стрим");
    card(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(
                    model
                        .stream
                        .started_at
                        .clone()
                        .unwrap_or_else(|| "начнётся после первой синхронизации".to_owned()),
                )
                .color(COLOR_MUTED),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add_enabled(
                        model.stream.can_reset,
                        egui::Button::new("Начать новый стрим"),
                    )
                    .on_hover_text("Обнуляет счётчики стрима во всех пресетах")
                    .clicked()
                {
                    actions.push(Action::AskReset);
                }
            });
        });
        ui.add_space(12.0);
        let overlay = model.overlay;
        ui.horizontal(|ui| {
            stat(
                ui,
                "Вышел живым",
                &grouped(overlay.session_extractions),
                COLOR_ACCENT,
            );
            ui.add_space(28.0);
            stat(ui, "Погиб", &grouped(overlay.session_deaths), COLOR_DANGER);
            ui.add_space(28.0);
            stat(
                ui,
                "Вынесено",
                &grouped(overlay.session_loot_value),
                COLOR_LOOT,
            );
            ui.add_space(28.0);
            stat(
                ui,
                "Баланс",
                &grouped_signed(overlay.session_money_delta),
                if overlay.session_money_delta < 0 {
                    COLOR_DANGER
                } else {
                    COLOR_ACCENT
                },
            );
        });
        ui.add_space(6.0);
        ui.label(
            RichText::new("Счётчики переживают перезапуск игры, ARC Live и Windows.")
                .small()
                .color(COLOR_MUTED),
        );
    });

    if model.stream.confirm_reset {
        ui.add_space(10.0);
        card(ui, |ui| {
            ui.label(
                RichText::new("Обнулить статистику текущего стрима?")
                    .strong()
                    .color(COLOR_LOOT),
            );
            ui.label("Она обнулится сразу во всех пресетах. Отменить будет нельзя.");
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("Да, начать новый стрим").clicked() {
                    actions.push(Action::ConfirmReset);
                }
                if ui.button("Отмена").clicked() {
                    actions.push(Action::CancelReset);
                }
            });
        });
    }

    section(ui, "События за сегодня");
    card(ui, |ui| {
        if model.events.is_empty() {
            ui.label(RichText::new("Событий пока нет").color(COLOR_MUTED));
        }
        for event in model.events.iter().take(8) {
            ui.horizontal_wrapped(|ui| {
                let color = match event.level.as_str() {
                    "success" => COLOR_ACCENT,
                    "warning" | "error" => COLOR_LOOT,
                    _ => COLOR_MUTED,
                };
                ui.label(RichText::new(&event.time).monospace().color(color));
                ui.label(&event.message);
            });
        }
    });

    ui.add_space(16.0);
    ui.collapsing("Если что-то не работает", |ui| {
        ui.label("Собери безопасный архив с диагностикой и передай его в поддержку. Токены, TLS-ключи и содержимое ответов игры в него не попадают.");
        ui.add_space(6.0);
        if ui.button("Сохранить диагностику").clicked() {
            actions.push(Action::ExportDiagnostics);
        }
    });
}

/// Explains a capture problem in the user's terms, with the exact next step.
fn capture_problem_card(
    ui: &mut egui::Ui,
    problem: CaptureProblem,
    capture: &CaptureView,
    launchers: &[String],
    actions: &mut Vec<Action>,
) {
    egui::Frame::group(ui.style())
        .inner_margin(egui::Margin::same(16))
        .corner_radius(egui::CornerRadius::same(10))
        .fill(Color32::from_rgb(48, 30, 24))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            let (title, explanation, step) = match problem {
                CaptureProblem::GameWithoutKeys => (
                    "Игра запущена без ключей шифрования",
                    "ARC Live прочитала окружение игры: переменной SSLKEYLOGFILE там нет. Игра берёт её у лаунчера, а он был запущен раньше, чем ARC Live её установила.",
                    "Полностью выйдите из Steam или Epic — через трей, а не просто закрыв окно, — затем перезагрузите Windows и запустите игру заново.",
                ),
                CaptureProblem::GameKeysElsewhere => (
                    "Игра пишет ключи в другой файл",
                    "У игры задан свой путь SSLKEYLOGFILE, и он не совпадает с файлом, который читает ARC Live.",
                    "Уберите свою переменную SSLKEYLOGFILE, полностью выйдите из лаунчера и перезагрузите Windows — ARC Live пропишет свой путь сама.",
                ),
                CaptureProblem::KeysDoNotMatch => (
                    "Ключи не подходят к соединениям игры",
                    "Соединения с сервером статистики видны, но расшифровать их нечем: ключи в файле относятся к другим программам.",
                    "Полностью выйдите из лаунчера, перезагрузите Windows и запустите игру заново.",
                ),
            };
            ui.label(RichText::new(title).size(17.0).strong().color(COLOR_LOOT));
            ui.add_space(4.0);
            ui.label(explanation);
            ui.add_space(6.0);
            // With a launcher at hand the button does the whole job, so the
            // manual reboot is only offered when there is nothing to press.
            if launchers.is_empty() {
                ui.label(RichText::new(step).strong());
            } else {
                ui.label(
                    RichText::new(
                        "Нажмите кнопку ниже — ARC Live закроет лаунчер и запустит его заново уже с ключами.",
                    )
                    .strong(),
                );
                ui.label(
                    RichText::new(format!("Если не поможет: {step}"))
                        .small()
                        .color(COLOR_MUTED),
                );
            }
            ui.add_space(6.0);
            if !launchers.is_empty() {
                ui.add_space(4.0);
                ui.horizontal_wrapped(|ui| {
                    for (index, title) in launchers.iter().enumerate() {
                        if ui
                            .button(format!("Перезапустить {title} со статистикой"))
                            .on_hover_text(
                                "ARC Live закроет лаунчер и запустит его заново уже с ключами. \
                                 Перезагрузка Windows не нужна.",
                            )
                            .clicked()
                        {
                            actions.push(Action::RestartLauncher(index));
                        }
                    }
                });
                ui.label(
                    RichText::new("После перезапуска лаунчера запустите игру как обычно.")
                        .small()
                        .color(COLOR_MUTED),
                );
            }
            ui.add_space(6.0);
            ui.label(
                RichText::new(format!(
                    "Рукопожатий с сервером: {} · ключей не подошло: {} · расшифровано: {}",
                    capture.handshakes, capture.key_errors, capture.decrypted
                ))
                .small()
                .color(COLOR_MUTED),
            );
        });
}

fn stat(ui: &mut egui::Ui, label: &str, value: &str, color: Color32) {
    ui.vertical(|ui| {
        ui.label(RichText::new(value).size(26.0).strong().color(color));
        ui.label(RichText::new(label).small().color(COLOR_MUTED));
    });
}

fn widget_page(ui: &mut egui::Ui, model: &ViewModel<'_>, actions: &mut Vec<Action>) {
    let overlay = model.overlay;
    let language = overlay.language.clone();

    ui.label(
        RichText::new("Так виджет выглядит в OBS")
            .size(17.0)
            .strong(),
    );
    ui.add_space(8.0);
    preview(ui, overlay, &language);

    section(ui, "Пресеты");
    if let Some(error) = model.preset_error {
        card(ui, |ui| {
            ui.label(
                RichText::new("Файл пресетов не принят — работает последний рабочий набор")
                    .color(COLOR_DANGER)
                    .strong(),
            );
            ui.label(RichText::new(error).small().color(COLOR_MUTED));
        });
        ui.add_space(8.0);
    }
    for preset in &overlay.presets {
        let selected = preset.id == overlay.preset;
        let response = egui::Frame::group(ui.style())
            .inner_margin(egui::Margin::symmetric(12, 9))
            .corner_radius(egui::CornerRadius::same(9))
            .fill(if selected {
                ui.style().visuals.selection.bg_fill.gamma_multiply(0.30)
            } else {
                Color32::TRANSPARENT
            })
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    dot(
                        ui,
                        if selected { COLOR_ACCENT } else { COLOR_MUTED },
                        selected,
                    );
                    ui.add_space(2.0);
                    ui.allocate_ui_with_layout(
                        egui::vec2(190.0, 20.0),
                        egui::Layout::left_to_right(egui::Align::Center),
                        |ui| {
                            ui.add(
                                egui::Label::new(
                                    RichText::new(preset.name(&language)).size(15.0).strong(),
                                )
                                .truncate(),
                            );
                        },
                    );
                    for cell in &preset.cells {
                        let (value, color) = cell_text(ui, cell);
                        ui.label(RichText::new(value).size(15.0).strong().color(color));
                        ui.label(
                            RichText::new(cell.label(&language))
                                .small()
                                .color(COLOR_MUTED),
                        );
                        ui.add_space(10.0);
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(
                            RichText::new(format!("?preset={}", preset.id))
                                .small()
                                .monospace()
                                .color(COLOR_MUTED.gamma_multiply(0.7)),
                        );
                        if selected {
                            ui.label(RichText::new("в эфире").small().color(COLOR_ACCENT));
                        }
                    });
                });
            })
            .response
            .interact(egui::Sense::click())
            .on_hover_cursor(egui::CursorIcon::PointingHand);
        if response.clicked() && !selected {
            actions.push(Action::SelectPreset(preset.id.clone()));
        }
        ui.add_space(4.0);
    }
    ui.horizontal_wrapped(|ui| {
        if ui.button("Открыть файл пресетов").clicked() {
            actions.push(Action::OpenPresetFile);
        }
        if ui.button("Перезагрузить").clicked() {
            actions.push(Action::ReloadPresets);
        }
        ui.label(
            RichText::new("В файле добавляются свои пресеты и меняются числа с подписями.")
                .small()
                .color(COLOR_MUTED),
        );
    });

    section(ui, "Подключение к OBS");
    card(ui, |ui| {
        ui.label("Добавь Browser Source с этим адресом один раз:");
        ui.add_space(6.0);
        ui.horizontal_wrapped(|ui| {
            ui.monospace(model.obs_url);
            if ui.button("Скопировать").clicked() {
                actions.push(Action::CopyObsUrl);
            }
            if ui.button("Открыть в браузере").clicked() {
                actions.push(Action::OpenObsPreview);
            }
        });
        ui.add_space(4.0);
        ui.label(
            RichText::new("Размер источника: 700 × 80. Пресет для конкретного источника можно закрепить в адресе.")
                .small()
                .color(COLOR_MUTED),
        );
    });

    section(ui, "Оформление");
    appearance(ui, overlay, actions);

    section(ui, "Проверка без игры");
    card(ui, |ui| {
        ui.horizontal_wrapped(|ui| {
            if overlay.mode == "demo" {
                if ui.button("Вернуться к реальным данным").clicked() {
                    actions.push(Action::SetDemo(false));
                }
                if ui.button("Баланс + / −").clicked() {
                    actions.push(Action::FlipDemoBalance);
                }
            } else if ui.button("Показать тестовые данные").clicked() {
                actions.push(Action::SetDemo(true));
            }
            ui.label(
                RichText::new("Позволяет разложить сцену в OBS до рейда.")
                    .small()
                    .color(COLOR_MUTED),
            );
        });
    });
}

fn preview(ui: &mut egui::Ui, overlay: &OverlayStats, language: &str) {
    let [red, green, blue] = overlay.background_color;
    let background = Color32::from_rgba_unmultiplied(
        red,
        green,
        blue,
        ((u16::from(overlay.opacity) * 255) / 100) as u8,
    );
    egui::Frame::new()
        .fill(Color32::from_rgb(28, 30, 33))
        .corner_radius(egui::CornerRadius::same(10))
        .inner_margin(egui::Margin::same(14))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            egui::Frame::new()
                .fill(background)
                .stroke(egui::Stroke::new(1.0, Color32::from_white_alpha(45)))
                .corner_radius(egui::CornerRadius::same(8))
                .inner_margin(egui::Margin::same(12))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    let Some(preset) = overlay.active_preset() else {
                        ui.label(
                            RichText::new("В файле пресетов нет показателей").color(COLOR_MUTED),
                        );
                        return;
                    };
                    if preset.cells.is_empty() {
                        ui.label(RichText::new("В пресете нет показателей").color(COLOR_MUTED));
                        return;
                    }
                    let rendered: Vec<(String, Color32, String)> = preset
                        .cells
                        .iter()
                        .map(|cell| {
                            let (value, mut color) = cell_text(ui, cell);
                            if cell.style.as_str() == "plain" || cell.style.is_empty() {
                                color = Color32::WHITE;
                            }
                            (value, color, cell.label(language).to_owned())
                        })
                        .collect();
                    ui.columns(rendered.len(), |columns| {
                        for (column, (value, color, label)) in
                            columns.iter_mut().zip(rendered.iter())
                        {
                            column.label(RichText::new(value).size(30.0).strong().color(*color));
                            column
                                .label(RichText::new(label).size(11.0).color(Color32::LIGHT_GRAY));
                        }
                    });
                });
            ui.add_space(6.0);
            ui.label(
                RichText::new("Так источник выглядит поверх сцены OBS")
                    .small()
                    .color(COLOR_MUTED),
            );
        });
}

fn appearance(ui: &mut egui::Ui, overlay: &OverlayStats, actions: &mut Vec<Action>) {
    card(ui, |ui| {
        ui.horizontal_wrapped(|ui| {
            ui.label("Язык:");
            for (code, title) in [("ru", "Русский"), ("en", "English")] {
                let selected = overlay.language == code;
                if ui.selectable_label(selected, title).clicked() && !selected {
                    actions.push(Action::SetLanguage(code.to_owned()));
                }
            }
        });
        ui.add_space(10.0);
        ui.horizontal_wrapped(|ui| {
            ui.label("Фон:");
            for (id, title, color, opacity, blur) in [
                ("transparent", "Прозрачный", [9, 16, 21], 0, 0),
                ("smoke", "Дым", [9, 16, 21], 48, 4),
                ("glass", "Стекло", [16, 30, 36], 32, 12),
                ("solid", "Плотный", [8, 12, 15], 82, 0),
            ] {
                let selected = overlay.background_preset == id;
                if ui.selectable_label(selected, title).clicked() && !selected {
                    actions.push(Action::SetAppearance {
                        preset: id.to_owned(),
                        color,
                        opacity,
                        blur,
                    });
                }
            }
            if overlay.background_preset == "custom" {
                ui.label(RichText::new("свой").small().color(COLOR_ACCENT));
            }
        });
        ui.add_space(10.0);
        let mut color = overlay.background_color;
        let mut opacity = overlay.opacity;
        let mut blur = overlay.background_blur;
        let mut changed = false;
        ui.horizontal_wrapped(|ui| {
            ui.label("Свой цвет:");
            changed |= ui.color_edit_button_srgb(&mut color).changed();
            ui.add_space(8.0);
            ui.label("Непрозрачность:");
            changed |= ui
                .add(egui::Slider::new(&mut opacity, 0..=100).suffix("%"))
                .changed();
            ui.add_space(8.0);
            ui.label("Размытие:");
            changed |= ui
                .add(egui::Slider::new(&mut blur, 0..=20).suffix(" px"))
                .changed();
        });
        if changed {
            actions.push(Action::SetAppearance {
                preset: "custom".to_owned(),
                color,
                opacity,
                blur,
            });
        }
    });
}

fn settings_page(ui: &mut egui::Ui, model: &ViewModel<'_>, actions: &mut Vec<Action>) {
    ui.label(RichText::new("Обновления").size(17.0).strong());
    ui.add_space(8.0);
    card(ui, |ui| {
        let mut automatic = model.updates.automatic;
        if ui
            .checkbox(&mut automatic, "Проверять обновления автоматически")
            .changed()
        {
            actions.push(Action::SetAutoUpdates(automatic));
        }
        ui.add_space(8.0);
        ui.horizontal_wrapped(|ui| {
            ui.label("Канал:");
            for (id, title) in [("stable", "Стабильный"), ("beta", "Бета")] {
                let selected = model.updates.channel == id;
                if ui.selectable_label(selected, title).clicked() && !selected {
                    actions.push(Action::SetChannel(id.to_owned()));
                }
            }
        });
        ui.add_space(10.0);
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(
                    !model.updates.checking,
                    egui::Button::new("Проверить сейчас"),
                )
                .clicked()
            {
                actions.push(Action::CheckUpdates);
            }
            if model.updates.checking {
                ui.spinner();
                ui.label("Проверяем…");
            } else if let Some(version) = &model.updates.available {
                ui.label(
                    RichText::new(format!("Установлена {}, доступна {version}", model.version))
                        .color(COLOR_ACCENT)
                        .strong(),
                );
            } else {
                ui.label(
                    RichText::new(format!(
                        "Установлена {} — это последняя версия",
                        model.version
                    ))
                    .color(COLOR_MUTED),
                );
            }
        });
        if model.updates.available.is_some() && !model.updates.downloaded {
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(!model.updates.downloading, egui::Button::new("Скачать"))
                    .clicked()
                {
                    actions.push(Action::DownloadUpdate);
                }
                if model.updates.downloading {
                    ui.spinner();
                    ui.label("Скачиваем и проверяем подпись…");
                }
            });
        }
        if model.updates.downloaded {
            ui.add_space(8.0);
            ui.label(
                RichText::new("Обновление скачано и проверено")
                    .color(COLOR_ACCENT)
                    .strong(),
            );
            if model.updates.blocked_by_game {
                ui.label(
                    RichText::new("Установка станет доступна после закрытия игры.")
                        .color(COLOR_MUTED),
                );
            } else if ui.button("Установить и закрыть ARC Live").clicked() {
                actions.push(Action::InstallUpdate);
            }
        }
        if let Some(error) = &model.updates.error {
            ui.add_space(6.0);
            ui.label(RichText::new(error).color(COLOR_DANGER));
        }
    });

    section(ui, "Файлы");
    card(ui, |ui| {
        ui.label(
            "Статистика стрима и настройки лежат отдельно от программы и переживают обновления.",
        );
        ui.add_space(8.0);
        ui.horizontal_wrapped(|ui| {
            if ui.button("Папка с данными").clicked() {
                actions.push(Action::OpenDataFolder);
            }
            if ui.button("Файл пресетов").clicked() {
                actions.push(Action::OpenPresetFile);
            }
            if ui
                .button("Применить правки файлов")
                .on_hover_text(
                    "Перечитывает config.json и widget-config.json и применяет всё, что можно \
                     применить на ходу.",
                )
                .clicked()
            {
                actions.push(Action::ReloadConfig);
            }
        });
        ui.add_space(4.0);
        ui.label(
            RichText::new("Порт локального сервера меняется только при перезапуске приложения.")
                .small()
                .color(COLOR_MUTED),
        );
    });

    section(ui, "О программе");
    card(ui, |ui| {
        ui.label(format!("ARC Live {}", model.version));
        ui.label(
            RichText::new(if model.connection.service_privileged {
                "Фоновый компонент захвата: работает"
            } else {
                "Фоновый компонент захвата: portable-режим"
            })
            .color(COLOR_MUTED),
        );
        if !model.capture_backend.is_empty() {
            ui.label(
                RichText::new(format!("Движок захвата: {}", model.capture_backend))
                    .color(COLOR_MUTED),
            );
        }
    });

    section(ui, "Ответственность");
    card(ui, |ui| {
        ui.label(
            RichText::new("Независимый проект, не связанный с Embark Studios.")
                .strong()
                .color(COLOR_LOOT),
        );
        ui.add_space(6.0);
        for paragraph in DISCLAIMER_PARAGRAPHS.iter().skip(1) {
            ui.label(RichText::new(*paragraph).color(COLOR_MUTED));
            ui.add_space(6.0);
        }
    });
}

/// Convenience for the preview example and tests.
pub fn demo_presets() -> Vec<OverlayPreset> {
    fn cell(value: i64, ru: &str, en: &str, style: &str) -> OverlayCell {
        OverlayCell {
            value,
            label_ru: ru.to_owned(),
            label_en: en.to_owned(),
            style: style.to_owned(),
        }
    }
    vec![
        OverlayPreset {
            id: "account".to_owned(),
            name_ru: "Статистика аккаунта".to_owned(),
            name_en: "Account totals".to_owned(),
            cells: vec![
                cell(24, "Ноки игроков", "Player knocks", "plain"),
                cell(28_640, "Урон рейдерам", "Raider damage", "accent"),
                cell(428_750, "Вынесено", "Extracted value", "loot"),
            ],
        },
        OverlayPreset {
            id: "session".to_owned(),
            name_ru: "Текущий стрим".to_owned(),
            name_en: "Current stream".to_owned(),
            cells: vec![
                cell(6, "Ноки за стрим", "Stream knocks", "plain"),
                cell(3, "Успешные выходы", "Successful exits", "accent"),
                cell(-56_100, "Баланс", "Balance", "balance"),
            ],
        },
        OverlayPreset {
            id: "outcome".to_owned(),
            name_ru: "Победы | Поражения".to_owned(),
            name_en: "Win | Lose".to_owned(),
            cells: vec![
                cell(3, "Вышел живым", "Extracted alive", "accent"),
                cell(2, "Погиб", "Knocked out", "danger"),
            ],
        },
        OverlayPreset {
            id: "pve".to_owned(),
            name_ru: "PvE · лут и ARC".to_owned(),
            name_en: "PvE · loot and ARC".to_owned(),
            cells: vec![
                cell(128_400, "Вынесено за стрим", "Stream loot", "loot"),
                cell(36_900, "Урон аркам", "ARC damage", "accent"),
            ],
        },
        OverlayPreset {
            id: "pvp".to_owned(),
            name_ru: "PvP · ноки и урон".to_owned(),
            name_en: "PvP · knocks and damage".to_owned(),
            cells: vec![
                cell(6, "Ноки игроков", "Player knocks", "plain"),
                cell(18_700, "Урон игрокам", "Player damage", "danger"),
            ],
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connection(game_running: bool) -> ConnectionView {
        ConnectionView {
            game_running,
            stats_ready: false,
            launcher_prepared: true,
            service_privileged: true,
        }
    }

    #[test]
    fn silent_while_the_game_is_not_running() {
        let capture = CaptureView {
            handshakes: 55,
            key_errors: 55,
            game_keylog: GameKeylogStatus::Missing,
            ..Default::default()
        };
        assert_eq!(capture_problem(&connection(false), &capture), None);
    }

    #[test]
    fn silent_once_anything_decrypts() {
        let capture = CaptureView {
            handshakes: 55,
            key_errors: 12,
            decrypted: 3,
            game_keylog: GameKeylogStatus::Missing,
        };
        assert_eq!(capture_problem(&connection(true), &capture), None);
    }

    #[test]
    fn game_without_the_variable_wins_over_the_generic_reason() {
        let capture = CaptureView {
            handshakes: 55,
            key_errors: 55,
            decrypted: 0,
            game_keylog: GameKeylogStatus::Missing,
        };
        assert_eq!(
            capture_problem(&connection(true), &capture),
            Some(CaptureProblem::GameWithoutKeys)
        );
    }

    #[test]
    fn reports_a_foreign_keylog_path() {
        let capture = CaptureView {
            handshakes: 4,
            key_errors: 4,
            decrypted: 0,
            game_keylog: GameKeylogStatus::Different,
        };
        assert_eq!(
            capture_problem(&connection(true), &capture),
            Some(CaptureProblem::GameKeysElsewhere)
        );
    }

    /// The exact shape of the second diagnostics bundle: keys are present but
    /// belong to other applications.
    #[test]
    fn handshakes_without_matching_keys_are_explained() {
        let capture = CaptureView {
            handshakes: 55,
            key_errors: 55,
            decrypted: 0,
            game_keylog: GameKeylogStatus::Unknown,
        };
        assert_eq!(
            capture_problem(&connection(true), &capture),
            Some(CaptureProblem::KeysDoNotMatch)
        );
    }

    #[test]
    fn quiet_start_is_not_reported_as_a_problem() {
        let capture = CaptureView {
            game_keylog: GameKeylogStatus::Matches,
            ..Default::default()
        };
        assert_eq!(capture_problem(&connection(true), &capture), None);
    }
}
