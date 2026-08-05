---
number: 649
title: fix(daemon): background worker の panic を検知せず該当機能が無通知で停止する
status: in-progress
priority: high
labels: [review, v2, daemon, reliability]
dependson: []
related: []
created_at: 2026-08-05T01:01:58.871445+00:00
updated_at: 2026-08-05T08:27:55.941259+00:00
---

## 出典

先行する "uiux" review session（origin/main 3e21b392 時点、コード変更なしのレビュー）の finding 4。本 issue はその finding を再検証し起票したもの。

## Finding

`src/runtime/daemon.rs` は 6 種類の長寿命 background worker を `std::thread::Builder::new().name(...)` で起動している: PR refresh（`spawn_pr_refresh_worker`）、session teardown（`spawn_session_teardown_worker`）、custody（`spawn_custody_worker`）、retention GC（`spawn_retention_gc_worker`）、draining collection（`spawn_draining_collection_worker`）、decision maintenance（`spawn_decision_maintenance`）。

これらを起動する `start_*` 関数はいずれも返ってきた `JoinHandle` を `.map(|_| ())` で即座に捨てており、以後どこからも保持・join・`is_finished()` による監視がされていない。

プロセス全体に対する panic hook（`install_panic_logger`）は存在し、panic 発生時にペイロードを `ErrorLog` に1行記録する。しかしこれはログに残すだけで、スレッドを再起動したり daemon の健全性シグナルに反映したりはしない。IPC accept loop や per-connection worker には別途 join/reap の仕組みがあるが、この 6 つの長寿命メンテナンスループは対象外である。

各ループは `while !shutdown.is_requested() { ... }` の形をしており、内部で一度でも panic すればスレッドはそのまま静かに死に、以後 daemon プロセス自体は動き続けるが、その機能（PR refresh / session teardown / custody 自己回収 / retention GC / draining generation の回収 / decision の締切処理）は**プロセス再起動までユーザーに見えない形で完全に停止する**。

## 影響

- 特に custody（lost-custody 時の自己回収セーフティネット）と session teardown（worktree clean up）が無通知で止まると、リソースリークや不整合が daemon 再起動まで蓄積する可能性がある。
- 現時点でこれが実際にインシデントを起こした形跡はないが、セーフティネットそのものにセーフティネットが無い状態。

## 修正方針（例）

- 各 worker の `JoinHandle` を保持し、daemon のヘルスチェック/metrics に「該当 worker が生存しているか」を反映する。
- panic した worker を再起動するか、少なくとも health indicator（`daemon_health.rs`）に新しい `HealthReason` として表面化する。

## 受け入れ条件

- 6 つの worker いずれかが panic した場合、daemon のヘルス情報（metrics または daemon modal）にその事実が反映される（panic を注入する integration test で確認する）。
- 通常運用時のオーバーヘッドが無視できる範囲であること。
