use anyhow::{Context, Result};
use serde_json::Value;

use crate::state::OverlayStats;

const DAMAGE_BY_ENEMY: i64 = 100;
const DAMAGE_BY_TARGET: i64 = 101;
const DAMAGE_BY_WEAPON: i64 = 102;
const KILLS_BY_TARGET: i64 = 200;
const PLAYER_DOWNS: i64 = 204;
const REVIVES: i64 = 400;
const CONTAINERS_LOOTED: i64 = 501;
const ITEMS_CRAFTED: i64 = 600;
const RAIDS: i64 = 9800;
const EXTRACTIONS: i64 = 9801;
const KNOCKED_OUT: i64 = 9802;
const DURATION_MS: i64 = 9803;
const VALUE_BROUGHT_IN: i64 = 9804;
const VALUE_EXTRACTED: i64 = 9805;
const XP_GAINED: i64 = 9902;
const PLAYER_TARGET_ID: i64 = 995_408_715;
const PLAYER_DAMAGE_TARGET_ID: i64 = 200_993_951;

pub fn normalize_player_stats(value: &Value) -> Result<(OverlayStats, u64)> {
    let scopes = value
        .get("scopedPlayerStats")
        .and_then(Value::as_array)
        .context("player stats response is missing scopedPlayerStats")?;
    let rows = scopes
        .first()
        .and_then(|scope| scope.get("playerStats"))
        .and_then(Value::as_array)
        .context("player stats response is missing scopedPlayerStats[0].playerStats")?;

    let (mut stats, unknown) = normalize_rows(rows);
    stats.mode = "live".to_owned();
    stats.preset = "account".to_owned();
    stats.stats_rows = rows.len() as u64;

    if let Some(today) = scopes.iter().skip(1).find(|scope| is_today_scope(scope))
        && let Some(today_rows) = today.get("playerStats").and_then(Value::as_array)
    {
        let (today_stats, _) = normalize_rows(today_rows);
        stats.today_extractions = today_stats.extractions;
        stats.today_deaths = today_stats.deaths;
        stats.today_raw_totals = today_stats.raw_totals;
        stats.today_available = true;
    }
    Ok((stats, unknown))
}

fn normalize_rows(rows: &[Value]) -> (OverlayStats, u64) {
    let mut stats = OverlayStats::default();
    let mut unknown = 0u64;
    for row in rows {
        let Some(event_id) = integer(row.get("eventId")) else {
            unknown = unknown.saturating_add(1);
            continue;
        };
        let amount = unsigned(row.get("amount")).unwrap_or_default();
        let target_id = integer(row.get("targetId"));
        add_raw_total(&mut stats, event_id, None, amount);
        if let Some(target_id) = target_id {
            add_raw_total(&mut stats, event_id, Some(target_id), amount);
        }
        match event_id {
            DAMAGE_BY_ENEMY => {
                add(&mut stats.damage_by_enemy, amount);
            }
            DAMAGE_BY_TARGET
                if !matches!(
                    target_id,
                    Some(PLAYER_TARGET_ID) | Some(PLAYER_DAMAGE_TARGET_ID)
                ) =>
            {
                add(&mut stats.raider_damage, amount)
            }
            DAMAGE_BY_TARGET => {}
            DAMAGE_BY_WEAPON => add(&mut stats.damage_by_weapon, amount),
            KILLS_BY_TARGET if target_id == Some(PLAYER_TARGET_ID) => {
                add(&mut stats.eliminations, amount)
            }
            KILLS_BY_TARGET => add(&mut stats.arc_eliminations, amount),
            PLAYER_DOWNS => add(&mut stats.downs, amount),
            REVIVES => add(&mut stats.revives, amount),
            CONTAINERS_LOOTED => add(&mut stats.containers_looted, amount),
            ITEMS_CRAFTED => add(&mut stats.items_crafted, amount),
            RAIDS => add(&mut stats.raids, amount),
            EXTRACTIONS => add(&mut stats.extractions, amount),
            KNOCKED_OUT => add(&mut stats.deaths, amount),
            DURATION_MS => add(&mut stats.duration_ms, amount),
            VALUE_BROUGHT_IN => add(&mut stats.value_brought_in, amount),
            VALUE_EXTRACTED => add(&mut stats.loot_value, amount),
            XP_GAINED => add(&mut stats.xp_gained, amount),
            _ => unknown = unknown.saturating_add(1),
        }
    }
    (stats, unknown)
}

fn add_raw_total(stats: &mut OverlayStats, event_id: i64, target_id: Option<i64>, amount: u64) {
    let key = target_id.map_or_else(
        || format!("event.{event_id}"),
        |target_id| format!("event.{event_id}.target.{target_id}"),
    );
    let total = stats.raw_totals.entry(key).or_default();
    *total = total.saturating_add(amount);
}

fn is_today_scope(scope: &Value) -> bool {
    let discriminant = scope
        .get("scope")
        .and_then(|value| {
            value
                .as_str()
                .or_else(|| value.get("discriminant").and_then(Value::as_str))
        })
        .unwrap_or_default();
    let normalized: String = discriminant
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect();
    matches!(
        normalized.as_str(),
        "day" | "daily" | "today" | "currentday" | "calendarday"
    )
}

fn add(target: &mut u64, amount: u64) {
    *target = target.saturating_add(amount);
}

fn integer(value: Option<&Value>) -> Option<i64> {
    let value = value?;
    value
        .as_i64()
        .or_else(|| value.as_u64().and_then(|number| i64::try_from(number).ok()))
        .or_else(|| value.as_str()?.parse().ok())
}

fn unsigned(value: Option<&Value>) -> Option<u64> {
    let value = value?;
    value
        .as_u64()
        .or_else(|| value.as_i64().and_then(|number| u64::try_from(number).ok()))
        .or_else(|| {
            value
                .as_f64()
                .filter(|number| *number >= 0.0)
                .map(|number| number as u64)
        })
        .or_else(|| {
            value
                .as_str()?
                .parse::<f64>()
                .ok()
                .filter(|number| *number >= 0.0)
                .map(|number| number as u64)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn normalizes_aggregate_scope_for_obs() {
        let response = json!({
            "scopedPlayerStats": [{
                "scope": "all",
                "playerStats": [
                    {"eventId": 9800, "targetId": 1, "amount": 7},
                    {"eventId": "9801", "targetId": 1, "amount": "4"},
                    {"eventId": 9802, "targetId": 1, "amount": 3},
                    {"eventId": 200, "targetId": PLAYER_TARGET_ID, "amount": 11},
                    {"eventId": 200, "targetId": 42, "amount": 29},
                    {"eventId": 9805, "targetId": 1, "amount": 123456},
                    {"eventId": 9902, "targetId": 1, "amount": 900},
                    {"eventId": 100, "targetId": PLAYER_DAMAGE_TARGET_ID, "amount": 1234}
                    ,{"eventId": 101, "targetId": 42, "amount": 5678}
                    ,{"eventId": 101, "targetId": PLAYER_TARGET_ID, "amount": 321}
                ]
            }, {
                "scope": {"discriminant": "Daily"},
                "playerStats": [
                    {"eventId": 9801, "targetId": 1, "amount": 2},
                    {"eventId": 9802, "targetId": 1, "amount": 1}
                ]
            }]
        });

        let (stats, unknown) = normalize_player_stats(&response).unwrap();
        assert_eq!(stats.raids, 7);
        assert_eq!(stats.extractions, 4);
        assert_eq!(stats.deaths, 3);
        assert_eq!(stats.eliminations, 11);
        assert_eq!(stats.arc_eliminations, 29);
        assert_eq!(stats.loot_value, 123_456);
        assert_eq!(stats.xp_gained, 900);
        assert_eq!(stats.raider_damage, 5_678);
        assert_eq!(stats.raw_totals["event.101"], 5_999);
        assert_eq!(stats.raw_totals["event.101.target.42"], 5_678);
        assert_eq!(stats.stats_rows, 10);
        assert_eq!(stats.today_extractions, 2);
        assert_eq!(stats.today_deaths, 1);
        assert!(stats.today_available);
        assert_eq!(unknown, 0);
    }

    #[test]
    fn rejects_missing_aggregate_scope() {
        assert!(normalize_player_stats(&json!({})).is_err());
    }
}
