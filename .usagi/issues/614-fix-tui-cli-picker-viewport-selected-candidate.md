---
number: 614
title: fix(tui): CLI picker viewport を selected candidate に追従させる
status: done
priority: medium
labels: [review, v2, tui, ux, render, correctness]
dependson: []
related: [578]
created_at: 2026-07-31T06:00:00+00:00
updated_at: 2026-08-01T00:41:45.061176+00:00
---

## Finding（P2 TUI）

`crates/tui/src/presentation/views/workspace_agent_drawer.rs::drawer_body` の Choosing renderer は `candidates.iter().enumerate().take(content_capacity)` で常に先頭だけを描く。一方 reducer は selected index を全候補上で進めるため、低い terminal で selected が viewport 外へ移動すると marker が消え、Enter が不可視の CLI を launch する。#1371 merge 後は同 symbol が `views/director_drawer.rs` に rename される。

## 最小修正方針

selected index を必ず含む bounded viewport start を計算し、上下に未表示候補があることも示す。render と pointer/hit-test が同じ viewport mapping を共有する。

## テストと受け入れ条件

- capacity より多い候補で first/middle/last selection が常に marker 付きで表示される。
- Up/Down wrap/clamp の現行 reducer契約と Enter の launch target が表示行に一致する。
- height 0〜最小 footer height で panicせず、候補/indicator/footer の優先順位が deterministic。
