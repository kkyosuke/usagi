---
number: 766
title: chore: nursery lint（redundant_clone / cognitive_complexity）の指摘を返済し allow を棚卸しする
status: done
priority: low
labels: [v2, refactor, lint]
dependson: []
related: [758, 760, 761]
created_at: 2026-09-16T00:00:00+00:00
updated_at: 2026-09-16T19:07:52.044279+00:00
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
| allow 総数 | 268 | 262 |
| 理由コメントの無い allow | 77 | **0** |

**6 件は不要になっていた**（#758 / #760 / #761 の関数分割で lint が発火しなくなったもの）。残りは
lint 名の言い換えではなく「その item がその形である理由」を書いた。

`document/06-conventions.md` の Git Hooks 節に、allow へ理由コメントを必須とする運用を追記した。

### この方法で 1 件取りこぼした（CI で検出・修正済み）

「削除して clippy が要求したものだけ戻す」という手順は **host の target でしか判定できない**。
`crates/daemon/src/infrastructure/unix_transport.rs` の `verify_owned_socket_stat` は `libc` の `mode_t` 幅が
target で違うため、同じ `as u32` が macOS では widening（`cast_lossless`）、Linux では no-op
（`unnecessary_cast`）になる。手元（macOS）の clippy は `unnecessary_cast` を出さないので不要と判定して削除し、
**Linux の CI で落ちた**。両方を allow に戻し、target 依存であることをコメントに明記した。

同じ取りこぼしを避けるため、「allow が不要かどうかをローカルの clippy 1 回で判断しない」ことも
`06-conventions.md` に書いた。削除した他の 15 件は target 非依存の lint で、この問題は無い。

### `cognitive_complexity`

当初の 39 件のうち production は 18 件…という計測は誤りで、test コードを含んでいた（#760 の本文で訂正済み）。
production の実数は、本スタックの分割前が 12 件、分割後が **9 件**である。tui controller の 2 件
（`update_event` cc 61 ほか）は #761 で解消した。

## 完了条件（達成状況）

- [x] `redundant_clone` 0 件
- [x] 理由コメントの無い `#[allow(clippy::…)]` が production コードに無い
- [~] production の `cognitive_complexity` 25 超が 5 件以下 — 9 件まで。残りは
  `drive_workspace_controller`（#758 の残件）と、本スタックの対象外ファイル
  （`views/config.rs` / `work_run_control.rs` / `generation.rs` / `orchestration.rs` / `workflow.rs` /
  `src/runtime/tui.rs`）にある。いずれも該当ファイルを扱う変更と一緒に返済するのが自然で、lint だけのために
  無関係なファイルを触るのは避けた。
