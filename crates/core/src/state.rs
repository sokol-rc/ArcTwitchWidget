use std::collections::{BTreeMap, VecDeque};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CollectorPhase {
    Starting,
    WaitingForLauncher,
    WaitingForGame,
    Capturing,
    TokenReady,
    WatchingRounds,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActivityItem {
    pub at: DateTime<Utc>,
    pub level: String,
    pub message: String,
}

/// One value rendered in a widget preset cell.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct OverlayCell {
    pub value: i64,
    pub label_ru: String,
    pub label_en: String,
    /// Rendering hint: `plain`, `accent`, `danger`, `loot` or `balance`.
    /// `balance` renders an explicit sign and colors by sign.
    pub style: String,
}

/// A user-defined widget layout. The list comes from `widget-config.json`, so
/// presets can be added, removed and renamed without a new executable.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct OverlayPreset {
    pub id: String,
    pub name_ru: String,
    pub name_en: String,
    pub cells: Vec<OverlayCell>,
}

impl OverlayPreset {
    pub fn name(&self, language: &str) -> &str {
        if language == "en" {
            &self.name_en
        } else {
            &self.name_ru
        }
    }
}

impl OverlayCell {
    pub fn label(&self, language: &str) -> &str {
        if language == "en" {
            &self.label_en
        } else {
            &self.label_ru
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct OverlayStats {
    pub mode: String,
    pub preset: String,
    pub language: String,
    pub opacity: u8,
    pub background_preset: String,
    pub background_color: [u8; 3],
    pub background_blur: u8,
    pub raids: u64,
    pub extractions: u64,
    pub eliminations: u64,
    pub deaths: u64,
    pub loot_value: u64,
    pub arc_eliminations: u64,
    pub downs: u64,
    pub revives: u64,
    pub containers_looted: u64,
    pub items_crafted: u64,
    pub damage_by_enemy: u64,
    pub damage_by_weapon: u64,
    pub raider_damage: u64,
    pub duration_ms: u64,
    pub value_brought_in: u64,
    pub xp_gained: u64,
    pub stats_rows: u64,
    pub session_downs: u64,
    pub session_extractions: u64,
    pub session_deaths: u64,
    pub session_loot_value: u64,
    pub session_money_delta: i64,
    pub today_extractions: u64,
    pub today_deaths: u64,
    pub today_available: bool,
    /// Complete numeric aggregate catalog from the account scope. Keys use
    /// `event.<id>` and `event.<id>.target.<id>` notation.
    pub raw_totals: BTreeMap<String, u64>,
    pub session_raw_totals: BTreeMap<String, u64>,
    pub today_raw_totals: BTreeMap<String, u64>,
    /// Every preset defined in widget-config.json, already resolved to numbers.
    /// The overlay renders `preset`; the others are shipped so a Browser Source
    /// can override the selection with `?preset=`.
    pub presets: Vec<OverlayPreset>,
}

impl OverlayStats {
    /// The preset the widget is currently rendering.
    pub fn active_preset(&self) -> Option<&OverlayPreset> {
        self.presets
            .iter()
            .find(|preset| preset.id == self.preset)
            .or_else(|| self.presets.first())
    }

    pub fn apply_session_baseline(&mut self, baseline: &Self) {
        self.session_downs = self.downs.saturating_sub(baseline.downs);
        self.session_extractions = self.extractions.saturating_sub(baseline.extractions);
        self.session_deaths = self.deaths.saturating_sub(baseline.deaths);
        self.session_loot_value = self.loot_value.saturating_sub(baseline.loot_value);
        let extracted = i128::from(self.session_loot_value);
        let invested = i128::from(
            self.value_brought_in
                .saturating_sub(baseline.value_brought_in),
        );
        self.session_money_delta =
            (extracted - invested).clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64;
        self.session_raw_totals = self
            .raw_totals
            .iter()
            .map(|(key, value)| {
                (
                    key.clone(),
                    value.saturating_sub(baseline.raw_totals.get(key).copied().unwrap_or_default()),
                )
            })
            .collect();
    }

    pub fn hydrate_legacy_raw_totals(&mut self) {
        if !self.raw_totals.is_empty() {
            return;
        }
        self.raw_totals
            .insert("event.200.target.995408715".into(), self.eliminations);
        self.raw_totals.insert("event.204".into(), self.downs);
        self.raw_totals
            .insert("event.9801".into(), self.extractions);
        self.raw_totals.insert("event.9802".into(), self.deaths);
        self.raw_totals
            .insert("event.9804".into(), self.value_brought_in);
        self.raw_totals.insert("event.9805".into(), self.loot_value);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OverlaySnapshot {
    pub schema_version: u8,
    pub updated_at: DateTime<Utc>,
    pub game_running: bool,
    pub stats: OverlayStats,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppState {
    pub version: String,
    pub phase: CollectorPhase,
    pub keylog_path: String,
    pub local_url: String,
    pub database_ready: bool,
    pub launcher_prepared: bool,
    pub game_running: bool,
    pub stats_stream_ready: bool,
    pub packets_seen: u64,
    pub tcp_443_segments: u64,
    pub tcp_443_to_server: u64,
    pub tcp_443_to_client: u64,
    pub keylog_entries: usize,
    pub tls_records: u64,
    pub tls_records_to_server: u64,
    pub tls_records_to_client: u64,
    pub tls_client_hellos: u64,
    pub tls_server_hellos: u64,
    pub tls_keys_established: u64,
    pub tls_client_hellos_with_keys: u64,
    pub tls_key_errors: u64,
    pub tls_decrypt_errors: u64,
    pub last_tls_sni: Option<String>,
    pub last_embark_sni: Option<String>,
    pub regional_api_hosts: Vec<String>,
    pub decrypted_records: u64,
    pub observations: u64,
    pub active_capture_connections: usize,
    pub capture_buffered_bytes: usize,
    pub capture_connections_evicted: u64,
    pub overlay: OverlayStats,
    pub last_update: DateTime<Utc>,
    pub activity: VecDeque<ActivityItem>,
}

impl AppState {
    pub fn new(version: impl Into<String>, keylog_path: impl Into<String>) -> Self {
        Self {
            version: version.into(),
            phase: CollectorPhase::Starting,
            keylog_path: keylog_path.into(),
            local_url: "http://127.0.0.1:17842".to_owned(),
            database_ready: false,
            launcher_prepared: false,
            game_running: false,
            stats_stream_ready: false,
            packets_seen: 0,
            tcp_443_segments: 0,
            tcp_443_to_server: 0,
            tcp_443_to_client: 0,
            keylog_entries: 0,
            tls_records: 0,
            tls_records_to_server: 0,
            tls_records_to_client: 0,
            tls_client_hellos: 0,
            tls_server_hellos: 0,
            tls_keys_established: 0,
            tls_client_hellos_with_keys: 0,
            tls_key_errors: 0,
            tls_decrypt_errors: 0,
            last_tls_sni: None,
            last_embark_sni: None,
            regional_api_hosts: Vec::new(),
            decrypted_records: 0,
            observations: 0,
            active_capture_connections: 0,
            capture_buffered_bytes: 0,
            capture_connections_evicted: 0,
            overlay: OverlayStats {
                mode: "live".to_owned(),
                preset: "account".to_owned(),
                language: "ru".to_owned(),
                opacity: 55,
                background_preset: "smoke".to_owned(),
                background_color: [9, 16, 21],
                background_blur: 6,
                ..Default::default()
            },
            last_update: Utc::now(),
            activity: VecDeque::new(),
        }
    }

    pub fn record(&mut self, level: &str, message: impl Into<String>) {
        self.last_update = Utc::now();
        self.activity.push_front(ActivityItem {
            at: self.last_update,
            level: level.to_owned(),
            message: message.into(),
        });
        self.activity.truncate(100);
    }

    pub fn overlay_snapshot(&self) -> OverlaySnapshot {
        OverlaySnapshot {
            schema_version: 7,
            updated_at: self.last_update,
            game_running: self.game_running,
            stats: self.overlay.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlay_contract_is_versioned() {
        let state = AppState::new("test", "private.keys");
        let value = serde_json::to_value(state.overlay_snapshot()).unwrap();
        assert_eq!(value["schema_version"], 7);
        assert!(value["stats"].get("raids").is_some());
        assert!(value["stats"].get("xp_gained").is_some());
        assert_eq!(value["stats"]["preset"], "account");
        assert_eq!(value["stats"]["language"], "ru");
        assert_eq!(value["stats"]["opacity"], 55);
        assert_eq!(value["stats"]["background_preset"], "smoke");
        assert_eq!(
            value["stats"]["background_color"],
            serde_json::json!([9, 16, 21])
        );
        assert_eq!(value["stats"]["background_blur"], 6);
        assert!(value["stats"]["presets"].is_array());
        assert!(value.get("keylog_path").is_none());
    }

    #[test]
    fn active_preset_falls_back_to_the_first_entry() {
        let mut stats = OverlayStats {
            preset: "gone".to_owned(),
            ..Default::default()
        };
        assert!(stats.active_preset().is_none());
        stats.presets = vec![
            OverlayPreset {
                id: "first".to_owned(),
                ..Default::default()
            },
            OverlayPreset {
                id: "second".to_owned(),
                ..Default::default()
            },
        ];
        assert_eq!(stats.active_preset().unwrap().id, "first");
        stats.preset = "second".to_owned();
        assert_eq!(stats.active_preset().unwrap().id, "second");
    }

    #[test]
    fn preset_and_cell_labels_follow_the_widget_language() {
        let preset = OverlayPreset {
            id: "pve".to_owned(),
            name_ru: "PvE".to_owned(),
            name_en: "PvE mode".to_owned(),
            cells: vec![OverlayCell {
                value: 5,
                label_ru: "Лут".to_owned(),
                label_en: "Loot".to_owned(),
                style: "loot".to_owned(),
            }],
        };
        assert_eq!(preset.name("ru"), "PvE");
        assert_eq!(preset.name("en"), "PvE mode");
        assert_eq!(preset.cells[0].label("ru"), "Лут");
        assert_eq!(preset.cells[0].label("en"), "Loot");
    }

    #[test]
    fn session_stats_are_deltas_from_the_first_sync() {
        let baseline = OverlayStats {
            downs: 10,
            extractions: 20,
            deaths: 30,
            loot_value: 40_000,
            value_brought_in: 8_000,
            ..Default::default()
        };
        let mut current = OverlayStats {
            downs: 13,
            extractions: 22,
            deaths: 31,
            loot_value: 55_000,
            value_brought_in: 11_000,
            ..Default::default()
        };
        current.apply_session_baseline(&baseline);
        assert_eq!(current.session_downs, 3);
        assert_eq!(current.session_extractions, 2);
        assert_eq!(current.session_deaths, 1);
        assert_eq!(current.session_loot_value, 15_000);
        assert_eq!(current.session_money_delta, 12_000);
    }

    #[test]
    fn session_money_delta_can_be_negative() {
        let baseline = OverlayStats {
            loot_value: 50_000,
            value_brought_in: 20_000,
            ..Default::default()
        };
        let mut current = OverlayStats {
            loot_value: 55_000,
            value_brought_in: 32_000,
            ..Default::default()
        };
        current.apply_session_baseline(&baseline);
        assert_eq!(current.session_money_delta, -7_000);
    }
}
