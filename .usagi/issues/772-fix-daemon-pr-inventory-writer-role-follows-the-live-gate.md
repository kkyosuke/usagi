---
number: 772
title: "fix(daemon): PR inventory の writer role が live gate に追従しない"
status: todo
priority: medium
labels: [v2, daemon, generation, pr-inventory, correctness]
dependson: []
related: [555, 562, 770]
created_at: 2026-09-25T00:00:00+00:00
updated_at: 2026-09-25T00:00:00+00:00
---

## 問題

PR inventory は whole-snapshot document なので、書いてよい generation は 1 つだけである
（[document/05-daemon.md#PR 検出の投影](../../document/05-daemon.md#pr-検出の投影)）。その判定は
`usecase::resources::fence::shared_write_verdict(writer, role)` が持ち、`draining` の writer を拒否する。

ところが `FencedPrInventory` は `spawn_ipc_server` で **`GenerationRole::Active` を定数として渡して構築**され、
その role は生成後に一度も更新されない。`AdmissionGate` の role が `draining` へ移っても projector は
`active` のつもりのまま書き続けるため、handoff 後に旧 generation と新 generation の 2 つが同じ
`daemon/pr-inventory.json` を上書きしうる。`shared_write_verdict` は他に consumer を持たないので、
拒否そのものが実質的に効いていない。

同じ形で `supervisor-runs/runs.index.json` も store lock なしに書かれている。

これらは data directory 側の document で workspace fence の内側ではないため、#770（置き換えられた世代が起動
workspace の fence を返す）は本件を作らず、広げもしない。別件として扱う。

## 方針（案）

writer role を構築時の定数ではなく `AdmissionGate` の live な role から引く（projector が gate を持ち、
書き込みのたびに `shared_write_verdict` を評価する）。`handed_off()` を判定に使えば、pre-commit barrier で
書き込みを止めてしまう問題も避けられる。

## 受入条件

- handoff が durable になった generation の projector が PR inventory への書き込みを拒否されることを test で
  固定する（構築時 role ではなく live gate を見ていること）。
- active generation の書き込みは従来どおり通る。
