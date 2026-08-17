//! Renders every ARC Live screen with synthetic data and writes PNG
//! screenshots, so the layout can be reviewed without running the real app.
//!
//! ```powershell
//! cargo run --example ui_preview -- <output directory>
//! ```

use std::path::PathBuf;

use arc_live::view::{
    CaptureView, ConnectionView, EventView, Page, StreamView, UpdateView, ViewModel, demo_presets,
    render,
};
use arc_live_core::state::{GameKeylogStatus, OverlayStats};
use eframe::egui;

struct Preview {
    output: PathBuf,
    pages: Vec<(Page, &'static str)>,
    index: usize,
    frames_on_page: u32,
    requested: bool,
    overlay: OverlayStats,
    events: Vec<EventView>,
    launchers: Vec<String>,
}

fn main() -> eframe::Result<()> {
    let output = std::env::args()
        .nth(1)
        .map_or_else(|| PathBuf::from("."), PathBuf::from);
    std::fs::create_dir_all(&output).expect("creating screenshot directory");

    let mut overlay = OverlayStats {
        mode: "live".to_owned(),
        preset: "pve".to_owned(),
        language: "ru".to_owned(),
        opacity: 48,
        background_preset: "smoke".to_owned(),
        background_color: [9, 16, 21],
        background_blur: 4,
        session_extractions: 3,
        session_deaths: 2,
        session_loot_value: 128_400,
        session_money_delta: -56_100,
        stats_rows: 1_587,
        ..Default::default()
    };
    overlay.presets = demo_presets();

    let preview = Preview {
        output,
        pages: vec![
            (Page::Stream, "01-stream"),
            (Page::Widget, "02-widget"),
            (Page::Settings, "03-settings"),
            (Page::Stream, "04-stream-no-keys"),
            (Page::Stream, "05-disclaimer"),
        ],
        index: 0,
        frames_on_page: 0,
        requested: false,
        overlay,
        launchers: vec!["Steam".to_owned()],
        events: vec![
            EventView {
                time: "14:51".into(),
                level: "success".into(),
                message: "Статистика обновлена после возвращения в Сперанцу".into(),
            },
            EventView {
                time: "14:44".into(),
                level: "success".into(),
                message: "Статистика текущего стрима восстановлена".into(),
            },
            EventView {
                time: "14:40".into(),
                level: "info".into(),
                message: "Игра запущена — подключаем статистику автоматически".into(),
            },
            EventView {
                time: "13:33".into(),
                level: "info".into(),
                message: "ARC Live запущена".into(),
            },
        ],
    };

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([980.0, 760.0]),
        ..Default::default()
    };
    eframe::run_native(
        "ARC Live UI preview",
        options,
        Box::new(|_| Ok(Box::new(preview))),
    )
}

impl eframe::App for Preview {
    fn ui(&mut self, root: &mut egui::Ui, _frame: &mut eframe::Frame) {
        arc_live::view::install_theme(root.ctx());
        let Some(&(page, name)) = self.pages.get(self.index) else {
            root.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        };

        if name == "05-disclaimer" {
            let _ = arc_live::view::disclaimer(root, false);
            self.capture_frame(root, name);
            return;
        }

        let model = ViewModel {
            page,
            version: "0.14.0",
            capture_backend: "raw socket",
            overlay: &self.overlay,
            connection: ConnectionView {
                game_running: true,
                stats_ready: name != "04-stream-no-keys",
                launcher_prepared: true,
                service_privileged: true,
            },
            stream: StreamView {
                started_at: Some("сегодня с 01:36".to_owned()),
                can_reset: true,
                confirm_reset: false,
            },
            events: &self.events,
            obs_url: "http://127.0.0.1:17842/overlay/live",
            updates: UpdateView {
                automatic: true,
                channel: "stable".to_owned(),
                ..Default::default()
            },
            launchers: &self.launchers,
            capture: if name == "04-stream-no-keys" {
                CaptureView {
                    handshakes: 55,
                    key_errors: 55,
                    decrypted: 0,
                    game_keylog: GameKeylogStatus::Missing,
                }
            } else {
                CaptureView {
                    handshakes: 12,
                    key_errors: 0,
                    decrypted: 340,
                    game_keylog: GameKeylogStatus::Matches,
                }
            },
            preset_error: None,
        };
        let _ = render(root, &model);
        self.capture_frame(root, name);
    }
}

impl Preview {
    /// Gives egui a few frames to settle the layout, then grabs the frame.
    fn capture_frame(&mut self, root: &egui::Ui, name: &str) {
        self.frames_on_page += 1;
        if self.frames_on_page > 3 && !self.requested {
            self.requested = true;
            root.ctx()
                .send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default()));
        }
        let shot = root.ctx().input(|input| {
            input.events.iter().find_map(|event| match event {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        if let Some(image) = shot {
            let path = self.output.join(format!("{name}.png"));
            write_png(&path, &image);
            println!("SCREENSHOT {}", path.display());
            self.index += 1;
            self.frames_on_page = 0;
            self.requested = false;
        }
        root.ctx().request_repaint();
    }
}

fn write_png(path: &std::path::Path, image: &egui::ColorImage) {
    let mut rgba = Vec::with_capacity(image.width() * image.height() * 4);
    for pixel in &image.pixels {
        rgba.extend_from_slice(&pixel.to_array());
    }
    let file = std::fs::File::create(path).expect("creating screenshot");
    let mut encoder = png::Encoder::new(
        std::io::BufWriter::new(file),
        image.width() as u32,
        image.height() as u32,
    );
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder
        .write_header()
        .expect("writing png header")
        .write_image_data(&rgba)
        .expect("writing png data");
}
