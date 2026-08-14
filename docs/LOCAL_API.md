# ARC Live local API

The server binds only to `127.0.0.1`. Its default port is `17842` and can be
changed in `%LOCALAPPDATA%\ARC Live\config.json`.

## Endpoints

- `GET /api/v1/health` — collector health and version.
- `GET /api/v1/snapshot` — sanitized collector state.
- `GET /api/v1/overlay` — stable, versioned OBS data contract.
- `GET /api/v1/observations?limit=100` — sanitized discovery observations.
- `GET /ws` — state snapshots and live `state.updated` messages.
- `GET /overlay/live` — production stats Browser Source.
- `GET /overlay/discovery` — collector diagnostics Browser Source.

## Overlay schema v6

```json
{
  "schema_version": 6,
  "updated_at": "2026-08-13T20:00:00Z",
  "game_running": true,
  "stats": {
    "mode": "live",
    "preset": "account",
    "language": "ru",
    "opacity": 55,
    "background_preset": "smoke",
    "background_color": [9, 16, 21],
    "background_blur": 6,
    "raids": 0,
    "extractions": 0,
    "eliminations": 0,
    "deaths": 0,
    "loot_value": 0,
    "arc_eliminations": 0,
    "downs": 0,
    "revives": 0,
    "containers_looted": 0,
    "items_crafted": 0,
    "damage_by_enemy": 0,
    "damage_by_weapon": 0,
    "raider_damage": 0,
    "duration_ms": 0,
    "value_brought_in": 0,
    "xp_gained": 0,
    "stats_rows": 0,
    "session_downs": 0,
    "session_extractions": 0,
    "session_deaths": 0,
    "session_loot_value": 0,
    "session_money_delta": 0,
    "today_extractions": 0,
    "today_deaths": 0,
    "today_available": false,
    "raw_totals": {
      "event.200.target.995408715": 123,
      "event.101": 4567
    },
    "session_raw_totals": {},
    "today_raw_totals": {},
    "widget_account": [123, 4444, 0],
    "widget_session": [0, 0, 0],
    "widget_outcome": [0, 0],
    "widget_pve": [0, 0],
    "widget_pvp": [0, 0]
  }
}
```

Existing field names and types remain compatible. The numeric event aggregates
are exposed so `widget-config.json` can select them; the raw Embark payload,
credentials, and account identifiers are not served. `/overlay/live` accepts
optional query parameters for a specific Browser Source: `preset=1..5`
(`4` is PvE and `5` is PvP),
`lang=ru|en`, `opacity=0..100`, `bg=RRGGBB`, and `blur=0..20`. These override
the selections made in the desktop app.
