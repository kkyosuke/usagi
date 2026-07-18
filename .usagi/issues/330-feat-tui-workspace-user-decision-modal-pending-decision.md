---
number: 330
title: feat(tui): workspace の user decision modal と pending decision 一覧を追加する
status: done
priority: high
labels: [tui, daemon, supervisor, ux]
dependson: [329]
related: [303, 314, 317, 328]
parent: 324
created_at: 2026-07-18T00:00:00+00:00
updated_at: 2026-07-18T02:05:27.725061+00:00
---

## 目的

対象 workspace の pending user decision を TUI modal で提示し、stable option または許可された自由記述を
daemon へ回答できるようにする。modal を閉じても pending decision は消さず、一覧から再表示できる。
設計の正本は [document/proposals/09-user-decision-mcp.md](../../document/proposals/09-user-decision-mcp.md) である。

## やること

- #329 の workspace-scoped snapshot/push を TUI backend と reconnect/resync に接続する。他 workspace の
  decision は projection に入れない。
- controller の pure state/event/effect に modal、pending list、再表示、option selection、freeform editor、
  submit/dismiss を追加する。dismiss は UI のみを閉じ、daemon の pending state を変えない。
- modal に title、prompt、option label/description、期限、freeform 許可時だけの入力欄を表示する。submit は
  stable option ID または text を daemon に送り、daemon confirmation event を受けてから一覧から除く。
- modal 中は terminal/closeup への入力を遮断する。disconnect、stale/duplicate response、resolve error は safe
  feedback と再試行可能な pending state にする。

## 受け入れ条件

- pending decision は対象 workspace で表示され、閉じた後も一覧から再表示できる。
- 未許可 freeform、空回答、不正 option を TUI が送らない。cancel/expire/resolved の event は表示を正しく収束する。
- restart/reconnect/resync と duplicate/stale response の reducer/render state を deterministic test で固定し、
  coverage 100% を維持する。

## 非目標

- supervisor を開始・resume する UI。回答は #329 の durable inbox event までである。

## テスト方針

- `cargo test -p usagi-tui`
- push/PR 前は full gate（coverage 100%）と Markdown link check。
