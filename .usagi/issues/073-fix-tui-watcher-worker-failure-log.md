---
number: 73
title: fix(tui): 監視スレッドの異常終了とワーカースレッドの panic をエラーログに記録する
status: done
priority: low
labels: [fix, tui, error-log]
dependson: []
related: [71]
created_at: 2026-06-21T00:00:03.000000+00:00
updated_at: 2026-06-21T03:23:20.664099+00:00
---

## 背景

バックグラウンドのスレッドで起きる失敗が握り潰され、エラーログに残らない:

- `src/presentation/tui/home/terminal_pool.rs` の watcher: `shared.lock()` が poison すると `Err(_) => break` で**黙って監視を停止**する。以後 bell / phase の反映が止まるが痕跡が残らない。
- 同ファイルのデスクトップ通知・`watcher.join()` などは `let _ =` で握り潰し。
- セッション作成・削除のワーカースレッド（`run_create` / `run_remove`）は `Err` は記録するが、**panic**（mutex poison の原因）はメッセージが残らない。`lock_session_ops` は poison から回復するだけ。

→ 監視やワーカーが死ぬとセッション表示が事実上機能停止するのに、原因を後から追えない。

## やること

- watcher の致命的停止（mutex poison での `break`）を `ErrorLog::record` で記録してから終了する。
- ワーカースレッドの panic を捕捉（`JoinHandle::join` の `Err`、または `catch_unwind`）し、panic ペイロードを記録する。
- 純粋な non-fatal な握り潰し（通知失敗・resize 失敗など）は対象外とし、ノイズを増やさない。
- `terminal_pool.rs` はカバレッジ除外（`scripts/coverage.sh`）対象のため、記録判断のロジックはテスト可能な層へ寄せられないか検討する。

## 確認方法

- watcher / ワーカーを故意に異常終了させたとき、`~/.usagi/logs/` に原因が記録されること。
- 正常系でノイズが増えないこと。
- `cargo fmt` / `cargo clippy --all-targets -- -D warnings` / `cargo test`（カバレッジ 100% 維持）。
