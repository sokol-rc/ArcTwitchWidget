use std::collections::HashSet;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::state::{OverlayCell, OverlayPreset, OverlayStats};

pub const WIDGET_CONFIG_SCHEMA_VERSION: u8 = 2;
pub const MAX_PRESETS: usize = 12;
pub const MAX_CELLS_PER_PRESET: usize = 4;
pub const CELL_STYLES: [&str; 5] = ["plain", "accent", "danger", "loot", "balance"];

/// User-editable widget layout. Presets are a free list: they can be added,
/// removed, renamed and reordered in `widget-config.json` without a new build.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WidgetConfig {
    pub schema_version: u8,
    pub presets: Vec<WidgetPreset>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct WidgetPreset {
    /// Stable identifier used by the app and by `?preset=<id>` in OBS.
    pub id: String,
    pub name_ru: String,
    pub name_en: String,
    pub cells: Vec<WidgetMetric>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct WidgetMetric {
    pub source: String,
    pub add: Vec<String>,
    pub subtract: Vec<String>,
    pub label_ru: String,
    pub label_en: String,
    /// One of `plain`, `accent`, `danger`, `loot`, `balance`.
    pub style: String,
}

impl Default for WidgetConfig {
    fn default() -> Self {
        Self {
            schema_version: WIDGET_CONFIG_SCHEMA_VERSION,
            presets: default_presets(),
        }
    }
}

/// The five presets ARC Live ships with. Order is also the `?preset=1..5` order.
fn default_presets() -> Vec<WidgetPreset> {
    vec![
        WidgetPreset {
            id: "account".into(),
            name_ru: "Статистика аккаунта".into(),
            name_en: "Account totals".into(),
            cells: vec![
                metric(
                    "account.event.200.target.995408715",
                    "Ноки игроков",
                    "Player knocks",
                    "plain",
                ),
                WidgetMetric {
                    source: "account.event.101".into(),
                    add: Vec::new(),
                    subtract: vec![
                        "account.event.101.target.995408715".into(),
                        "account.event.101.target.200993951".into(),
                    ],
                    label_ru: "Урон рейдерам".into(),
                    label_en: "Raider damage".into(),
                    style: "accent".into(),
                },
                metric("account.loot_value", "Вынесено", "Extracted value", "loot"),
            ],
        },
        WidgetPreset {
            id: "session".into(),
            name_ru: "Текущий стрим".into(),
            name_en: "Current stream".into(),
            cells: vec![
                metric(
                    "session.event.200.target.995408715",
                    "Ноки за стрим",
                    "Stream knocks",
                    "plain",
                ),
                metric(
                    "session.extractions",
                    "Успешные выходы",
                    "Successful exits",
                    "accent",
                ),
                metric("session.money_delta", "Баланс", "Balance", "balance"),
            ],
        },
        WidgetPreset {
            id: "outcome".into(),
            name_ru: "Победы | Поражения".into(),
            name_en: "Win | Lose".into(),
            cells: vec![
                metric("outcome.wins", "Вышел живым", "Extracted alive", "accent"),
                metric("outcome.losses", "Погиб", "Knocked out", "danger"),
            ],
        },
        WidgetPreset {
            id: "pve".into(),
            name_ru: "PvE · лут и ARC".into(),
            name_en: "PvE · loot and ARC".into(),
            cells: vec![
                metric(
                    "session.loot_value",
                    "Вынесено за стрим",
                    "Stream loot",
                    "loot",
                ),
                WidgetMetric {
                    source: "session.event.101".into(),
                    add: Vec::new(),
                    subtract: vec![
                        "session.event.101.target.995408715".into(),
                        "session.event.101.target.200993951".into(),
                    ],
                    label_ru: "Урон аркам".into(),
                    label_en: "ARC damage".into(),
                    style: "accent".into(),
                },
            ],
        },
        WidgetPreset {
            id: "pvp".into(),
            name_ru: "PvP · ноки и урон".into(),
            name_en: "PvP · knocks and damage".into(),
            cells: vec![
                metric(
                    "session.event.200.target.995408715",
                    "Ноки игроков",
                    "Player knocks",
                    "plain",
                ),
                WidgetMetric {
                    source: "session.event.101.target.995408715".into(),
                    add: vec!["session.event.101.target.200993951".into()],
                    subtract: Vec::new(),
                    label_ru: "Урон игрокам".into(),
                    label_en: "Player damage".into(),
                    style: "danger".into(),
                },
            ],
        },
    ]
}

fn metric(source: &str, label_ru: &str, label_en: &str, style: &str) -> WidgetMetric {
    WidgetMetric {
        source: source.into(),
        add: Vec::new(),
        subtract: Vec::new(),
        label_ru: label_ru.into(),
        label_en: label_en.into(),
        style: style.into(),
    }
}

impl WidgetConfig {
    pub fn load_or_create(path: &Path) -> Result<Self> {
        if !path.exists() {
            let config = Self::default();
            config.save(path)?;
            return Ok(config);
        }
        let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let document: Value = serde_json::from_slice(&bytes)
            .with_context(|| format!("parsing {}", path.display()))?;
        let version = document
            .get("schema_version")
            .and_then(Value::as_u64)
            .unwrap_or(1);
        let (config, migrated) = match version {
            0 | 1 => (Self::from_legacy_document(&document), true),
            2 => (
                serde_json::from_value(document)
                    .with_context(|| format!("parsing {}", path.display()))?,
                false,
            ),
            other => bail!("unsupported widget config schema {other}"),
        };
        config.validate()?;
        if migrated {
            config.save(path)?;
        }
        Ok(config)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        fs::write(path, serde_json::to_vec_pretty(self)?)
            .with_context(|| format!("saving {}", path.display()))
    }

    /// Converts the pre-0.12 fixed `account`/`session`/`outcome`/`pve`/`pvp`
    /// layout into the free preset list, keeping user-edited sources and labels.
    fn from_legacy_document(document: &Value) -> Self {
        let presets = default_presets()
            .into_iter()
            .map(|mut preset| {
                if let Some(cells) = document.get(&preset.id).and_then(Value::as_array) {
                    let migrated: Vec<WidgetMetric> = cells
                        .iter()
                        .zip(preset.cells.iter())
                        .filter_map(|(value, fallback)| {
                            let mut metric: WidgetMetric =
                                serde_json::from_value(value.clone()).ok()?;
                            if metric.style.trim().is_empty() {
                                metric.style = fallback.style.clone();
                            }
                            Some(metric)
                        })
                        .collect();
                    if !migrated.is_empty() {
                        preset.cells = migrated;
                    }
                }
                preset
            })
            .collect();
        Self {
            schema_version: WIDGET_CONFIG_SCHEMA_VERSION,
            presets,
        }
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == WIDGET_CONFIG_SCHEMA_VERSION,
            "unsupported widget config schema"
        );
        ensure!(!self.presets.is_empty(), "widget config has no presets");
        ensure!(
            self.presets.len() <= MAX_PRESETS,
            "widget config has more than {MAX_PRESETS} presets"
        );
        let mut seen = HashSet::new();
        for preset in &self.presets {
            let id = preset.id.trim();
            ensure!(!id.is_empty(), "preset id is empty");
            ensure!(
                id.chars()
                    .all(|character| character.is_ascii_alphanumeric() || character == '-'),
                "preset id \"{id}\" must use only latin letters, digits and hyphens"
            );
            ensure!(seen.insert(id), "preset id \"{id}\" is used twice");
            ensure!(
                !preset.name_ru.trim().is_empty() && !preset.name_en.trim().is_empty(),
                "preset \"{id}\" has an empty name"
            );
            ensure!(!preset.cells.is_empty(), "preset \"{id}\" has no values");
            ensure!(
                preset.cells.len() <= MAX_CELLS_PER_PRESET,
                "preset \"{id}\" has more than {MAX_CELLS_PER_PRESET} values"
            );
            for metric in &preset.cells {
                ensure!(
                    !metric.source.trim().is_empty(),
                    "a value in preset \"{id}\" has no source"
                );
                ensure!(
                    !metric.label_ru.trim().is_empty() && !metric.label_en.trim().is_empty(),
                    "a value in preset \"{id}\" has an empty label"
                );
                ensure!(
                    metric.style.trim().is_empty() || CELL_STYLES.contains(&metric.style.trim()),
                    "unknown style \"{}\" in preset \"{id}\"",
                    metric.style
                );
            }
        }
        Ok(())
    }

    /// Resolves every preset against the current totals and stores the result in
    /// the overlay contract. Also repairs the active preset if it was deleted.
    pub fn apply(&self, stats: &mut OverlayStats) {
        let presets: Vec<OverlayPreset> = self
            .presets
            .iter()
            .map(|preset| OverlayPreset {
                id: preset.id.trim().to_owned(),
                name_ru: preset.name_ru.clone(),
                name_en: preset.name_en.clone(),
                cells: preset
                    .cells
                    .iter()
                    .map(|metric| OverlayCell {
                        value: metric.resolve(stats),
                        label_ru: metric.label_ru.clone(),
                        label_en: metric.label_en.clone(),
                        style: if metric.style.trim().is_empty() {
                            "plain".to_owned()
                        } else {
                            metric.style.trim().to_owned()
                        },
                    })
                    .collect(),
            })
            .collect();
        if !presets.iter().any(|preset| preset.id == stats.preset)
            && let Some(first) = presets.first()
        {
            stats.preset = first.id.clone();
        }
        stats.presets = presets;
    }

    pub fn contains(&self, preset_id: &str) -> bool {
        self.presets.iter().any(|preset| preset.id == preset_id)
    }
}

impl WidgetMetric {
    fn resolve(&self, stats: &OverlayStats) -> i64 {
        let base = i128::from(resolve_source(&self.source, stats));
        let add = self
            .add
            .iter()
            .map(|source| i128::from(resolve_source(source, stats)))
            .sum::<i128>();
        let subtract = self
            .subtract
            .iter()
            .map(|source| i128::from(resolve_source(source, stats)))
            .sum::<i128>();
        (base + add - subtract).clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
    }
}

fn resolve_source(source: &str, stats: &OverlayStats) -> i64 {
    let (scope, name) = source.split_once('.').unwrap_or(("account", source));
    if name.starts_with("event.") {
        let value = match scope {
            "session" => stats.session_raw_totals.get(name),
            "today" => stats.today_raw_totals.get(name),
            _ => stats.raw_totals.get(name),
        };
        return value.copied().unwrap_or_default().min(i64::MAX as u64) as i64;
    }

    let unsigned = match (scope, name) {
        ("account", "raids") => stats.raids,
        ("account", "extractions") => stats.extractions,
        ("account", "eliminations") => stats.eliminations,
        ("account", "deaths") => stats.deaths,
        ("account", "loot_value") => stats.loot_value,
        ("account", "arc_eliminations") => stats.arc_eliminations,
        ("account", "downs") => stats.downs,
        ("account", "revives") => stats.revives,
        ("account", "damage_by_enemy") => stats.damage_by_enemy,
        ("account", "damage_by_weapon") => stats.damage_by_weapon,
        ("account", "raider_damage") => stats.raider_damage,
        ("account", "value_brought_in") => stats.value_brought_in,
        ("account", "xp_gained") => stats.xp_gained,
        ("session", "downs") => stats.session_downs,
        ("session", "extractions") => stats.session_extractions,
        ("session", "deaths") => stats.session_deaths,
        ("session", "loot_value") => stats.session_loot_value,
        ("today", "extractions") => stats.today_extractions,
        ("today", "deaths") => stats.today_deaths,
        ("outcome", "wins") => {
            if stats.session_extractions + stats.session_deaths == 0 && stats.today_available {
                stats.today_extractions
            } else {
                stats.session_extractions
            }
        }
        ("outcome", "losses") => {
            if stats.session_extractions + stats.session_deaths == 0 && stats.today_available {
                stats.today_deaths
            } else {
                stats.session_deaths
            }
        }
        ("session", "money_delta") => return stats.session_money_delta,
        _ => 0,
    };
    unsigned.min(i64::MAX as u64) as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stats_with(session_event_101: u64) -> OverlayStats {
        let mut stats = OverlayStats {
            preset: "pve".to_owned(),
            session_loot_value: 125_000,
            ..Default::default()
        };
        stats
            .session_raw_totals
            .insert("event.101".into(), session_event_101);
        stats
            .session_raw_totals
            .insert("event.101.target.995408715".into(), 9_000);
        stats
    }

    #[test]
    fn arbitrary_event_target_can_be_selected_without_code_changes() {
        let mut stats = OverlayStats {
            preset: "account".to_owned(),
            ..Default::default()
        };
        stats.raw_totals.insert("event.777.target.42".into(), 123);
        let mut config = WidgetConfig::default();
        config.presets[0].cells[0].source = "account.event.777.target.42".into();
        config.apply(&mut stats);
        assert_eq!(stats.presets[0].cells[0].value, 123);
    }

    #[test]
    fn pve_preset_uses_current_stream_deltas() {
        let mut stats = stats_with(42_000);
        WidgetConfig::default().apply(&mut stats);
        let pve = stats.active_preset().expect("pve preset resolved");
        assert_eq!(pve.id, "pve");
        assert_eq!(pve.cells[0].value, 125_000);
        assert_eq!(pve.cells[1].value, 33_000);
        assert_eq!(pve.cells[0].style, "loot");
    }

    #[test]
    fn user_can_add_a_preset_with_its_own_value_count() {
        let mut config = WidgetConfig::default();
        config.presets.push(WidgetPreset {
            id: "solo".into(),
            name_ru: "Только лут".into(),
            name_en: "Loot only".into(),
            cells: vec![metric(
                "session.loot_value",
                "Вынесено",
                "Extracted",
                "loot",
            )],
        });
        config.validate().unwrap();
        let mut stats = stats_with(1_000);
        stats.preset = "solo".to_owned();
        config.apply(&mut stats);
        let active = stats.active_preset().unwrap();
        assert_eq!(active.id, "solo");
        assert_eq!(active.cells.len(), 1);
        assert_eq!(active.cells[0].value, 125_000);
    }

    #[test]
    fn deleted_preset_falls_back_to_the_first_one() {
        let mut stats = stats_with(0);
        stats.preset = "removed".to_owned();
        WidgetConfig::default().apply(&mut stats);
        assert_eq!(stats.preset, "account");
    }

    #[test]
    fn rejects_duplicate_ids_and_unknown_styles() {
        let mut duplicated = WidgetConfig::default();
        duplicated.presets[1].id = "account".into();
        assert!(duplicated.validate().is_err());

        let mut styled = WidgetConfig::default();
        styled.presets[0].cells[0].style = "rainbow".into();
        assert!(styled.validate().is_err());
    }

    #[test]
    fn migrates_the_pre_0_12_fixed_layout_and_keeps_user_edits() {
        let legacy = serde_json::json!({
            "schema_version": 1,
            "account": [
                {"source": "account.event.777", "add": [], "subtract": [],
                 "label_ru": "Моё поле", "label_en": "My field"},
                {"source": "account.raider_damage", "add": [], "subtract": [],
                 "label_ru": "Урон рейдерам", "label_en": "Raider damage"},
                {"source": "account.loot_value", "add": [], "subtract": [],
                 "label_ru": "Вынесено", "label_en": "Extracted value"}
            ],
            "session": [], "outcome": [], "pve": [], "pvp": []
        });
        let root =
            std::env::temp_dir().join(format!("arc-live-widget-{:016x}", rand::random::<u64>()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("widget-config.json");
        std::fs::write(&path, serde_json::to_vec_pretty(&legacy).unwrap()).unwrap();

        let migrated = WidgetConfig::load_or_create(&path).unwrap();
        assert_eq!(migrated.schema_version, WIDGET_CONFIG_SCHEMA_VERSION);
        assert_eq!(migrated.presets.len(), 5);
        assert_eq!(migrated.presets[0].id, "account");
        assert_eq!(migrated.presets[0].cells[0].source, "account.event.777");
        assert_eq!(migrated.presets[0].cells[0].label_ru, "Моё поле");
        // A style the old format did not have is filled in from the defaults.
        assert_eq!(migrated.presets[0].cells[2].style, "loot");
        // Empty legacy sections keep the shipped defaults.
        assert_eq!(migrated.presets[3].id, "pve");
        assert_eq!(migrated.presets[3].cells.len(), 2);

        // The migration is written back, so the next start reads schema 2.
        let reloaded = WidgetConfig::load_or_create(&path).unwrap();
        assert_eq!(reloaded.presets.len(), 5);
        let _ = std::fs::remove_dir_all(root);
    }
}
