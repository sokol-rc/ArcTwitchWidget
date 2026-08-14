use std::fs;
use std::path::Path;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use crate::state::OverlayStats;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WidgetConfig {
    pub schema_version: u8,
    pub account: [WidgetMetric; 3],
    pub session: [WidgetMetric; 3],
    pub outcome: [WidgetMetric; 2],
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct WidgetMetric {
    pub source: String,
    pub subtract: Vec<String>,
    pub label_ru: String,
    pub label_en: String,
}

impl Default for WidgetConfig {
    fn default() -> Self {
        Self {
            schema_version: 1,
            account: [
                metric(
                    "account.event.200.target.995408715",
                    "Ноки игроков",
                    "Player knocks",
                ),
                WidgetMetric {
                    source: "account.event.101".into(),
                    subtract: vec![
                        "account.event.101.target.995408715".into(),
                        "account.event.101.target.200993951".into(),
                    ],
                    label_ru: "Урон рейдерам".into(),
                    label_en: "Raider damage".into(),
                },
                metric("account.loot_value", "Вынесено", "Extracted value"),
            ],
            session: [
                metric(
                    "session.event.200.target.995408715",
                    "Ноки за стрим",
                    "Stream knocks",
                ),
                metric("session.extractions", "Успешные выходы", "Successful exits"),
                metric("session.money_delta", "Баланс", "Balance"),
            ],
            outcome: [
                metric("outcome.wins", "Вышел живым", "Extracted alive"),
                metric("outcome.losses", "Погиб", "Knocked out"),
            ],
        }
    }
}

fn metric(source: &str, label_ru: &str, label_en: &str) -> WidgetMetric {
    WidgetMetric {
        source: source.into(),
        subtract: Vec::new(),
        label_ru: label_ru.into(),
        label_en: label_en.into(),
    }
}

impl WidgetConfig {
    pub fn load_or_create(path: &Path) -> Result<Self> {
        if !path.exists() {
            let config = Self::default();
            fs::write(path, serde_json::to_vec_pretty(&config)?)
                .with_context(|| format!("creating {}", path.display()))?;
            return Ok(config);
        }
        let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let config: Self = serde_json::from_slice(&bytes)
            .with_context(|| format!("parsing {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(self.schema_version == 1, "unsupported widget config schema");
        for metric in self
            .account
            .iter()
            .chain(self.session.iter())
            .chain(self.outcome.iter())
        {
            ensure!(!metric.source.trim().is_empty(), "widget source is empty");
            ensure!(
                !metric.label_ru.trim().is_empty(),
                "Russian widget label is empty"
            );
            ensure!(
                !metric.label_en.trim().is_empty(),
                "English widget label is empty"
            );
        }
        Ok(())
    }

    pub fn apply(&self, stats: &mut OverlayStats) {
        stats.widget_account = self.account.each_ref().map(|metric| metric.resolve(stats));
        stats.widget_session = self.session.each_ref().map(|metric| metric.resolve(stats));
        stats.widget_outcome = self.outcome.each_ref().map(|metric| metric.resolve(stats));
        stats.widget_account_labels_ru = self
            .account
            .each_ref()
            .map(|metric| metric.label_ru.clone());
        stats.widget_account_labels_en = self
            .account
            .each_ref()
            .map(|metric| metric.label_en.clone());
        stats.widget_session_labels_ru = self
            .session
            .each_ref()
            .map(|metric| metric.label_ru.clone());
        stats.widget_session_labels_en = self
            .session
            .each_ref()
            .map(|metric| metric.label_en.clone());
        stats.widget_outcome_labels_ru = self
            .outcome
            .each_ref()
            .map(|metric| metric.label_ru.clone());
        stats.widget_outcome_labels_en = self
            .outcome
            .each_ref()
            .map(|metric| metric.label_en.clone());
    }
}

impl WidgetMetric {
    fn resolve(&self, stats: &OverlayStats) -> i64 {
        let base = i128::from(resolve_source(&self.source, stats));
        let subtract = self
            .subtract
            .iter()
            .map(|source| i128::from(resolve_source(source, stats)))
            .sum::<i128>();
        (base - subtract).clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
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

    #[test]
    fn default_mapping_selects_player_knocks_and_excludes_player_damage() {
        let mut stats = OverlayStats::default();
        stats
            .raw_totals
            .insert("event.200.target.995408715".into(), 884);
        stats.raw_totals.insert("event.101".into(), 2_075_973);
        stats
            .raw_totals
            .insert("event.101.target.995408715".into(), 211_555);
        stats.loot_value = 81_326_044;

        WidgetConfig::default().apply(&mut stats);

        assert_eq!(stats.widget_account, [884, 1_864_418, 81_326_044]);
        assert_eq!(stats.widget_account_labels_ru[0], "Ноки игроков");
    }

    #[test]
    fn arbitrary_event_target_can_be_selected_without_code_changes() {
        let mut stats = OverlayStats::default();
        stats.raw_totals.insert("event.777.target.42".into(), 123);
        let mut config = WidgetConfig::default();
        config.account[0].source = "account.event.777.target.42".into();
        config.apply(&mut stats);
        assert_eq!(stats.widget_account[0], 123);
    }

    #[test]
    fn bundled_widget_config_is_valid() {
        let config: WidgetConfig =
            serde_json::from_str(include_str!("../../../widget-config.json")).unwrap();
        config.validate().unwrap();
        assert_eq!(
            config.account[0].source,
            "account.event.200.target.995408715"
        );
        assert_eq!(config.account[1].source, "account.event.101");
    }

    #[test]
    fn empty_profile_never_receives_preview_values() {
        let mut stats = OverlayStats::default();
        WidgetConfig::default().apply(&mut stats);

        assert_eq!(stats.widget_account, [0, 0, 0]);
        assert_eq!(stats.widget_session, [0, 0, 0]);
        assert_eq!(stats.widget_outcome, [0, 0]);
    }
}
