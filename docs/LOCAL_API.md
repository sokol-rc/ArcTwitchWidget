# ARC Live local API

The server binds only to `127.0.0.1`. Its preferred port is `17842`; if busy,
ARC Live selects and persists a free loopback port. The selected port can be
changed in `%LOCALAPPDATA%\ARC Live\config.json`.

## Endpoints

- `GET /api/v1/health` — collector health and version.
- `GET /api/v1/snapshot` — sanitized collector state.
- `GET /api/v1/overlay` — stable, versioned OBS data contract.
- `GET /api/v1/observations?limit=100` — sanitized discovery observations.
- `GET /ws` — state snapshots and live `state.updated` messages.
- `GET /overlay/live` — production stats Browser Source.
- `GET /overlay/discovery` — collector diagnostics Browser Source.

## Overlay schema v7

```json
{
  "schema_version": 7,
  "updated_at": "2026-08-15T20:00:00Z",
  "game_running": true,
  "stats": {
    "mode": "live",
    "preset": "pve",
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
    "presets": [
      {
        "id": "pve",
        "name_ru": "PvE · лут и ARC",
        "name_en": "PvE · loot and ARC",
        "cells": [
          {"value": 125000, "label_ru": "Вынесено за стрим",
           "label_en": "Stream loot", "style": "loot"},
          {"value": 33000, "label_ru": "Урон аркам",
           "label_en": "ARC damage", "style": "accent"}
        ]
      }
    ]
  }
}
```

### Что изменилось в v7

Фиксированные поля `widget_account`, `widget_session`, `widget_outcome`,
`widget_pve`, `widget_pvp` и десять массивов `widget_*_labels_*` заменены одним
массивом `presets`. Каждый пресет описан в пользовательском
`widget-config.json`, поэтому их число, имена и количество показателей задаёт
пользователь, а не сборка. `preset` содержит `id` активного пресета.

`style` показателя - подсказка отрисовки: `plain`, `accent` (зелёный),
`danger` (красный), `loot` (жёлтый) и `balance` (знак `+`/`−` и цвет по знаку).

Именованные агрегаты (`raids`, `loot_value`, `raw_totals` и прочие) не менялись.
Сырой ответ Embark, credentials и идентификаторы аккаунта по-прежнему не
публикуются.

`/overlay/live` принимает необязательные параметры для конкретного Browser
Source: `preset=<id>` либо `preset=<номер от 1>` в порядке из файла,
`lang=ru|en`, `opacity=0..100`, `bg=RRGGBB` и `blur=0..20`. Они перекрывают
выбор, сделанный в приложении.
