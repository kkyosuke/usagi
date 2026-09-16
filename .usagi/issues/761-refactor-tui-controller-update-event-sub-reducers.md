---
number: 761
title: refactor(tui): controller の update_event を event family 別の sub-reducer に分ける
status: todo
priority: medium
labels: [v2, refactor, tui]
dependson: []
related: [758]
created_at: 2026-09-16T00:00:00+00:00
updated_at: 2026-09-16T00:00:00+00:00
---

## 問題

`crates/tui/src/usecase/application/controller.rs`（6,084 行、`mod tests` は別ファイル 8,794 行）の Home reducer
`update_event` は 654 行、`clippy::cognitive_complexity` 61 である。入力語彙の `AppKey` は 55 variants、`Effect` は
35 variants、`presentation/views/key_help.rs` の `Context` は 44 variants あり、全 bounded context の event が 1 つの
`match` に集約されている。`update_editor_backend`（167 行）、`update_management_key`（142 行）も同じ形である。

architecture test `tui_controller_keeps_its_bounded_contexts_and_tests_out_of_the_home_reducer` は context module を
reducer 外に保っているが、reducer 本体の入口は依然として肥大している。

## 方針

- `AppEvent` / `AppKey` を context 別の enum（session / terminal / director / config / garden …）にネストし、
  `update_event` は context への振り分けだけを行う。
- 各 context の sub-reducer（`update_session_event` など）を `controller/<context>.rs` に置き、context 単位の
  reducer test を `controller/tests.rs` から対応ファイルへ移す。
- `Effect` も同じ軸で group 化し、executor 側の `match` を短くする。

## 完了条件

- `update_event` が 100 行以下、`cognitive_complexity` 25 以下。
- `controller/tests.rs` が context 別ファイルに分かれ、単一ファイルで 3,000 行を超えない。
