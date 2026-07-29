---
number: 580
title: fix(tui): Workspace Agent drawer の Ctrl-G と scope 排他を修正する
status: done
priority: high
labels: [tui, bug, input, agent, closeup]
dependson: []
related: [576, 577, 578]
created_at: 2026-07-29T22:25:14.139583+00:00
updated_at: 2026-07-29T22:40:09.533125+00:00
---

## 背景

Workspace Agent drawer の leader follow-up が plain `g` に割り当てられ、Closeup modal と drawer の同時表示を招く。root scope (`session_id: None`) の Agent が managed-session Closeup pane にも投影され、root New のたびに両 surface の tab が増える。`[ New ]` の production mouse-down も背景 pane に奪われ picker を開けない。ユーザー向け名称には内部機能名 `Workspace Agent` が露出している。

## 対象

- drawer chord を `Ctrl-O` → `Ctrl-G` に変更し、semantic control key / control character / raw byte を分類する。plain `g` は drawer action にせず PTY へ渡す。
- drawer を独立した frontmost input context とし、modal / background pane / live PTY へ同じ event を fallthrough させない。modal と drawer を同時 visible にしない。
- `[ New ]` の production mouse-down hit を drawer が先に消費し、picker の ↑↓ / Enter / Esc ownership と root foreground を維持する。
- root Agent と managed-session pane の restore / reconcile / request / completion admission / render scope を厳密に分離する。target と terminal scope が異なる completion を拒否する。
- root New を複数回実行しても managed pane の tab count / identity / selection / focus を変えない。
- Home header、drawer title、drawer footer / empty state のユーザー向け名称を既存 Nerd Font 方針に沿う robot glyph + `chat` に統一する。
- footer、`document/03-tui.md`、render/golden、production shell test を実装と一致させる。

## 受入条件

- [x] Closeup で `Ctrl-O` → plain `g` を押しても drawer は開かず、plain `g` は PTY へ 1 回だけ届く。
- [x] `Ctrl-O` → `Ctrl-G` は Switch / Closeup / live pane から drawer だけを toggle する。
- [x] modal→drawer、drawer→close、picker→Esc、picker→Enter の遷移は一意で、modal と drawer は同時 visible にならない。
- [x] `[ New ]` click 直後に picker が見え、背景 pane/tab の click/focus/attach を変更せず ↑↓ / Enter で確定できる。
- [x] root Agent は drawer のみに、managed-session Agent/Terminal は該当 Closeup pane のみに投影・admit される。
- [x] root New を複数回実行しても drawer conversations だけが増え、managed pane tab identity/count は不変である。
- [x] drawer は chord または `Esc` で閉じ、元の background route / pane selection / focus に戻る。
- [x] Home header と drawer surface は同じ robot glyph + `chat` 表示を使い、内部の型/action/機能名は維持する。
- [x] 入力分類、modal/drawer 排他、mouse hit、scope 分離、表示名の回帰テストと文書が更新される。
