---
number: 419
title: fix(cli): daemon エラー時に exit code 0 を返す経路を非 0 exit に変更する
status: todo
priority: medium
labels: [fix, cli, review]
dependson: []
related: []
created_at: 2026-07-20T11:57:01.791620+00:00
updated_at: 2026-07-20T11:57:01.791620+00:00
---

## 背景

v2 全体の 7 サブシステム並列コードレビュー（2026-07-20）由来。file:line は 2f4dc5b6 時点で検証済み。

## 根拠（検証済み）

- `src/main.rs:40-43` — `mcp` 分岐で daemon unavailable を stderr に出して `Ok(())` を返す。
- `src/runtime/cli.rs:45-48` — `daemon request failed` を stderr 出力後 `Ok(())`。`:50-53` — `daemon unavailable` も同様。
- `src/runtime/tui.rs:1595-1596` — stderr 出力後 `return Ok(())`。
- `tests/cli_tui.rs:126-152`（`cli_daemon_request_autostarts...`）が session remove 失敗時に `output.status.success()` を assert しており、**exit 0 の挙動をテストが固定**している。

## 問題

スクリプトや CI から usagi CLI を使うと、daemon 障害・リクエスト失敗が exit code で検知できない（`set -e` や `&&` 連鎖をすり抜ける）。

## 改善案（要検討）

- daemon エラー時は非 0 exit（例: 1）を返す。エラー種別ごとの code 割り当ては任意。
- `tests/cli_tui.rs` の該当 assert を新しい契約に更新する。
- TUI 対話面（tui.rs:1595）は「起動前の致命エラー」と「対話中の一時エラー」で扱いを分けるか検討する。

## 受け入れ条件

- [ ] daemon unavailable / request failed の CLI 経路が非 0 で終了する。
- [ ] テストが新契約（非 0 exit）を固定している。
- [ ] coverage 100% を維持する。
