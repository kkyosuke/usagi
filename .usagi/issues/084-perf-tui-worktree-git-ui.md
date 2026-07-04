---
number: 84
title: perf(tui): リネーム/ノート/並べ替えが全 worktree の git 同期を UI スレッドで実行し、セッション数に比例して固まる
status: done
priority: high
labels: [perf, tui, fix]
dependson: []
related: [62, 65]
created_at: 2026-07-03T20:07:44.440614+00:00
updated_at: 2026-07-03T20:28:28.888832+00:00
---

## 症状

セッションをたくさん開いている状態で、リネーム・ノート保存・並べ替え（`K`/`J`）を行うと TUI が数秒固まる。固まる時間はセッション数に比例する。

## 原因

`src/presentation/tui/home/mod.rs` の `rename_display` / `set_note` / `reorder_session` クロージャが、イベントループ（UI スレッド）上で `reload_sessions` → `workspace_state::sync` を同期呼び出ししている。`sync` は全セッション worktree に対して複数の git サブプロセスを fan-out し（rayon 並列でもセッション数に比例）、さらに `state.json` のプロセス間ロック取得を待つ。

周辺コメントは「synchronous (no git work to block on)」と主張しているが、実際には `reload_sessions` がフル git 同期を走らせており、コメントと実装が乖離している。特に並べ替えはキー 1 打鍵ごとに全 worktree の git 同期が走る。

## 対応

これらの操作は state.json のメタデータ（表示名・ノート・順序）しか変更せず、git の状態には影響しない。よって git 再同期は不要で、`workspace_state::recorded_sessions`（state.json の再読込のみ）で一覧を組み直せば十分。git ステータスの鮮度は既存の入場時・pane 離脱時のバックグラウンド同期が保つ。

## 確認方法

- セッション多数（10+）で `K`/`J` 連打・リネーム・ノート保存が即応すること。
- `cargo fmt` / `cargo clippy --all-targets -- -D warnings` / `cargo test`（カバレッジ 100% 維持）。
