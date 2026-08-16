---
number: 687
title: feat(tui): session garden のうさぎを agent 単位で描く
status: todo
priority: medium
labels: [v2, tui, uiux]
dependson: []
related: [674]
created_at: 2026-08-16T22:38:12.153428+00:00
updated_at: 2026-08-16T22:38:12.153428+00:00
---

## 背景

Garden の描画素材は現在 controller が集約した `TargetPhase` を 1 session につき 1 つ使い、1 session = 1 うさぎで描いている。1 session は agent を複数持てるため、この集約は羽数と「動いている agent」を落とす。集約は `Done > Waiting > Running > Ready > Absent` の最大ランクを採るので、「1 つ終了・1 つ実行中」の session は `Done` へ畳まれ、実行中の作業が休んでいるうさぎとして描かれる。

設計は `document/proposals/15-session-garden.md#うさぎは-agent区画は-session` が正本（#1491 で確定）。

## スコープ

- `GardenSession` を「session に属する agent ごとの phase + stable な runtime identity」を持つ形へ広げる。session の lifecycle は区画（nameplate と地面）、agent の phase は個々のうさぎが表す。
- agent が 1 つの session の見た目は変えない（1 羽を大きく描く）。複数持つ session だけが複数羽になる。
- 並び順は 注目度（`Waiting` を先頭）→ stable な runtime identity。同じ素材の frame で羽が入れ替わらないようにする。
- 表示上限を超えた分は区画に `+N` と畳む。畳むのは注目度の低いほうからで、`Waiting` の agent は必ず見える。
- plot の大きさは agent 数で変えない（区画ごとに幅が変わると grid の決定性と hit test が崩れる）。
- 状態ラベル（`2 run · 1 wait`）は色に依存せず内訳を読めるようにするため省かない。

## 含めないもの

- click の粒度。hitbox は区画 = session のまま、遷移先も session の Closeup に一本化しておく（うさぎ単位の hitbox は遷移先が増えるため別途決める）。
- workspace root の agent の描画。
- agent ごとの表示名（現在の runtime 参照は表示用 label を持たない）。

## 受け入れ条件

- 1 session に複数 agent があるとき、羽数と各 agent の phase が描かれ、集約によって実行中の agent が休んでいる姿に化けない。
- 表示上限を超えた agent は `+N` に畳まれ、`Waiting` の agent は畳まれずに必ず見える。
- agent の並びは phase と stable な runtime identity だけで決まり、同じ素材の frame では入れ替わらない。
- 同じ入力 snapshot / tick / size は byte-for-byte 同じ frame と hitbox を返す。
- 既存の click 遷移（区画 = session → その session の Closeup）と自動表示は壊れない。
- 見た目のために daemon schema、永続 session record、IPC event を増やさない。
