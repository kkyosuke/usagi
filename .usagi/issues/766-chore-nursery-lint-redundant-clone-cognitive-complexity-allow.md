---
number: 766
title: chore: nursery lint（redundant_clone / cognitive_complexity）の指摘を返済し allow を棚卸しする
status: done
priority: low
labels: [v2, refactor, lint]
dependson: []
related: [758, 760, 761]
created_at: 2026-09-16T00:00:00+00:00
updated_at: 2026-09-16T18:43:05.976335+00:00
---

## 問題

workspace は `clippy::all` + `clippy::pedantic` を warn にしているが、nursery の 2 lint を追加で回すと
`redundant_clone` 252 件、`cognitive_complexity`（閾値 25）39 件が出ていた。また pedantic を通すための
`#[allow(clippy::…)]` が 268 件あり、そのうち 77 件は理由コメントが無く「まだ要るのか」を判断できなかった。

## やったこと

### `redundant_clone` 252 → 0

clippy の machine-applicable な suggestion を全件適用し、その結果生じた `redundant_field_names` /
`uninlined_format_args` / `redundant_locals` も同じ方法で解消した。挙動は不変で、
`cargo test --workspace` で回帰が無いことを確認している。

### `#[allow(clippy::…)]` の棚卸し

理由コメントの無い 77 件をいったん**全部削除**し、clippy がまだ必要とするものだけを理由コメント付きで戻した。

| | before | after |
|---|---|---|
| allow 総数 | 268 | 261 |
| 理由コメントの無い allow | 77 | **0** |

**7 件は不要になっていた**（#758 / #760 / #761 の関数分割で lint が発火しなくなったもの）。残る 70 件は
lint 名の言い換えではなく「その item がその形である理由」を書いた。

`document/06-conventions.md` の Git Hooks 節に、allow へ理由コメントを必須とする運用を 1 行追記した。

### `cognitive_complexity`

当初の 39 件のうち production は 18 件…という計測は誤りで、test コードを含んでいた（#760 の本文で訂正済み）。
production の実数は、本スタックの分割前が 12 件、分割後が **9 件**である。

| 領域 | before | after |
|---|---|---|
| tui controller | 2（`update_event` 61 / `update_editor_backend`） | 0 |
| tui presentation | 4 | 4（うち `drive_workspace_controller` 129 は #758 の残件） |
| tui その他 | 3 | 3 |
| daemon | 3 | 3 |
| root | 2 | 2 |

## 完了条件（達成状況）

- [x] `redundant_clone` 0 件
- [x] 理由コメントの無い `#[allow(clippy::…)]` が production コードに無い
- [~] production の `cognitive_complexity` 25 超が 5 件以下 — 9 件まで。残りは
  `drive_workspace_controller`（#758 の残件）と、本スタックの対象外ファイル
  （`views/config.rs` / `work_run_control.rs` / `generation.rs` / `orchestration.rs` / `workflow.rs` /
  `src/runtime/tui.rs`）にある。いずれも該当ファイルを扱う変更と一緒に返済するのが自然で、lint だけのために
  無関係なファイルを触るのは避けた。
