---
number: 610
title: fix(tui): Config save を最新 settings への field merge にする
status: done
priority: high
labels: [review, v2, tui, config, persistence, data-loss, correctness]
dependson: []
related: [241]
created_at: 2026-07-31T15:00:00+09:00
updated_at: 2026-08-01T06:52:16+09:00
---

## Finding（P1 data loss）

`src/runtime/tui.rs::PersistentSettingsPort::save` は Config modal が保持する stale `Settings` snapshot で global `settings.json` 全体を `save_settings` し、workspace は `LocalSettings::from(settings)` を新規生成して保存する。後者は既存 `LocalSettings.env` を空 map にし、前者は modal open 後に environment editor / local LLM 等が更新した field を巻き戻す。`LocalSettings::with_config` は env を保持する意図で既に存在するが、save path が既存 local を load せず使っていない。

## 最小修正方針

scope lock 下で最新 document を再読込し、Config が所有する default model / issue / memory field だけを merge する。workspace は `existing.with_config(settings)`、global も dedicated patch/CAS で env・local LLM 等を保持する。

## テストと受け入れ条件

- workspace Config save 後も既存 local env が byte/semantic 同値で残る。
- modal open 後の concurrent global env/local LLM 更新を Config save が巻き戻さない。
- 同じ owned field の競合方針（CAS refusal または明示 last-writer）をテストと safe feedback で固定する。
