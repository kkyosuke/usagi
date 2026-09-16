---
number: 758
title: refactor(tui): presentation/mod.rs を bounded context 別 module に分割し frame loop の複雑度を下げる
status: done
priority: high
labels: [v2, refactor, tui]
dependson: []
related: [757, 759, 761]
created_at: 2026-09-16T00:00:00+00:00
updated_at: 2026-09-16T15:35:29.587084+00:00
---

## 問題

`crates/tui/src/presentation/mod.rs` は 9,906 行の production コード（`mod tests` は別ファイル）に top-level 関数 160、
型 73、`impl` 42 を抱え、直近 400 commit で 151 回変更されていた。`#[coverage(off)]` は 2 件だけなので、残り 158 関数は
テスト対象のロジックであり、architecture test が「composition module」と呼ぶ場所に複数の bounded context が同居していた。

## やったこと

bounded context ごとに 9 つの module へ分割した。

| module | 行 | 内容 |
|---|---|---|
| `frame_loop.rs` | 2,468 | 実端末の Home frame loop と screen graph 起動 |
| `terminal_io.rs` | 1,230 | pane / terminal の起動・入力転送・選択・投影 |
| `workspace_io.rs` | 770 | frame loop が使う daemon transport の調整役 |
| `restore.rs` | 630 | 復元 job と対象選定・再試行 |
| `flow_steps.rs` | 549 | Welcome / New / Open / Config の起動フロー |
| `director.rs` | 475 | Director drawer / tab の選択と projection |
| `session_commands.rs` | 400 | session コマンド発行と完了・snapshot 反映 |
| `work_run.rs` | 309 | Work run pane の入力と observation / control job |
| `garden.rs` | 252 | Garden の入力 routing と observation job |

`mod.rs` は 9,906 → 3,104 行（69% 減）。`tests/architecture.rs` の
`tui_presentation_keeps_tests_and_observation_policy_out_of_its_composition_module` を拡張し、9 module の存在と
「代表 symbol が composition module に戻っていないこと」を固定した。分割で参照先が変わった既存 guard
（`tui_application_runtime_ports_are_not_declared_by_presentation` /
`tui_presentation_discovers_session_catalogs_through_an_application_port`）も、presentation 層全体を見る形へ更新した。

## 残件

`drive_workspace_controller`（1,353 行 / `cognitive_complexity` 129）の内部分解は行っていない。この関数は約 130 行の
setup（30 近いローカル束縛）と 1 本の `loop` から成り、段ごとに分けるには状態を保持する struct を導入する設計変更が要る。
実端末の frame loop 本体であり `#[coverage(off)]` で PTY E2E だけが通る経路なので、module 分割と同じ PR で行うと
回帰の切り分けができなくなる。当初の完了条件のうち「300 行超の関数を無くす」「`cognitive_complexity` 100 超を無くす」は
この 1 関数のために未達で、別 issue として扱う。

## 完了条件（達成状況）

- [x] bounded context ごとの module 分割と architecture guard
- [x] `mod.rs` の大幅な縮小（9,906 → 3,104 行。当初目標の 1,500 行は `drive_workspace_controller` の分解が前提）
- [x] `document/02-architecture.md` のディレクトリ構成に分割後の module を追記
- [ ] `drive_workspace_controller` の段分割（上記のとおり別 issue）
