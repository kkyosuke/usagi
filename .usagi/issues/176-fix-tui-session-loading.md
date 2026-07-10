---
number: 176
title: fix(tui): session 削除中の loading を赤色にする
status: done
priority: medium
labels: [tui, bug]
dependson: []
related: []
created_at: 2026-07-10T23:19:17.625188+00:00
updated_at: 2026-07-10T23:32:03.923444+00:00
---

## 目的

TUI で session 削除処理中のインライン loading を赤色で表示し、作成など通常の loading と明確に区別する。

## 受け入れ条件

- session 削除中に対象行を置き換える loading skeleton の認識可能な表示全体（✂ と session 名の wave）が `danger`（赤）になる。
- session 作成中を含む通常の loading は従来の cyan/accent 系の見た目を維持する。
- ANSI スタイルを検証するテストで削除中と通常 loading の色をそれぞれ保証する。
- 現行の console 文字列描画経路に従い、ratatui Style は導入しない。

## 実装方針

現行 TUI の session 削除は右上の `loading_rabbit` ではなく、対象 session 行を置換する
`removing_session_rows` / `rail_removing_session_rows` が実表示である。両者が共有する wave helper を
`console::Style::danger` に変更し、共通の通常 loading widget や作成 skeleton は変更しない。
