---
number: 410
title: refactor(tui): 本番未配線の usecase 並行実装（~3,500 行）を配線するか削除するか決定する
status: todo
priority: high
labels: [refactor, tui, review]
dependson: []
related: []
created_at: 2026-07-20T11:54:07.022077+00:00
updated_at: 2026-07-20T11:54:07.022077+00:00
---

## 背景

v2 全体（v1/ 除く、~84k 行）の 7 サブシステム並列コードレビュー（2026-07-20）由来。file:line は 2f4dc5b6 時点の現行コードで検証済み。

`crates/tui/src/usecase/application/` 配下に、presentation 側の実装と同役割の**本番未配線の並行実装が約 3,500 行**存在する。テストと coverage 除外のコストだけ払い続けており、「配線して presentation 実装を置換する」か「削除する」かの意思決定が必要。

## 根拠（検証済み）

- `crates/tui/src/usecase/application/daemon_backend.rs`（873 行）: 自称 "Production executor"（:1）だが、`DaemonBackend::new` の呼び出しは自ファイル `#[cfg(test)]`（:566, :574）のみで**本番構築箇所ゼロ**。doc :16-17 が「no `#[coverage(off)]`」と明記した 2 行後の :19 に `#![coverage(off)]`（虚偽 doc）。
- `crates/tui/src/usecase/application/lifecycle.rs`（999 行）＋ `lifecycle_adapter.rs`（641 行）: 利用者は自ファイル内テストのみ。controller と同概念の `Target`（lifecycle.rs:23 / controller.rs:463）・`Selection`（:32 / :484）・`Mode`（:41）・`PendingOperation`（:70 / :522）を二重定義。
- `agent_launch.rs`（589 行）・`agent_runtime.rs`（415 行）・`terminal_launch.rs`（335 行）・`pane_runtime.rs`（614 行）: 外部からの実コード参照は `pane_runtime::Geometry`（terminal_session.rs:16、presentation/mod.rs:56）のみ。
- `crates/tui/src/usecase/application/controller.rs` 内の Entry reducer `update_entry`（:1618、領域 ~1463-1712）と New reducer `update_new`（:1942、領域 ~1720-2085）: 呼び出し元は coverage-off なテストヘルパ（:1701-1707, :2024-2032）と `#[cfg(test)]`（:3231 以降）のみで本番参照ゼロ。
- **同名 trait の並存**: `SessionCommandPort` が `daemon_backend.rs:105`（create/refresh/remove ＋ Completions）と `presentation/mod.rs:444`（`execute(workspace, selected, command)` 単一メソッド）で別シグネチャのまま 2 つ定義されている。

## 問題

- 実行されないコードにテスト・レビュー・リファクタのコストが掛かり続ける。
- 同名 trait・同概念型の二重定義が読み手を誤誘導する（"Production executor" の虚偽 doc は特に有害）。
- coverage 100% gate の実効性を下げる（一括 `#![coverage(off)]` の温床。棚卸し issue が後続）。

## 判断材料

- 配線する場合: daemon_backend.rs は controller の `Effect` ストリームを実 daemon に接続する設計で、presentation 側の port 群（`presentation/mod.rs` の SessionCommandPort ほか）を置換できる。reducer（Entry/New）も controller の新アーキテクチャ移行の途中成果物。
- 削除する場合: presentation 側の現行実装が実運用で動いており、機能欠損はない。削除で ~3,500 行と coverage 除外が消える。
- どちらでも `pane_runtime::Geometry` は利用中のため残す（移設可）。

## 改善案（要検討）

1. 配線 or 削除を決定する（本 issue のスコープは意思決定＋実行）。
2. 削除の場合: 上記モジュール群・未使用 reducer・重複 trait を落とし、`pane_runtime::Geometry` を利用箇所近くへ移す。
3. 配線の場合: `SessionCommandPort` を一本化し、presentation 側の同役割実装を段階的に置換する計画 issue に分割する。

## 受け入れ条件

- [ ] 配線/削除の決定が本 issue に記録されている。
- [ ] 決定に沿って未配線コードが解消され、同名 trait `SessionCommandPort` は 1 定義になる。
- [ ] daemon_backend.rs の虚偽 doc（"Production executor" / "no coverage(off)"）が実態と一致する（削除なら消滅）。
- [ ] coverage 100% を維持する。
