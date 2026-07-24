---
number: 543
title: fix(daemon): session remove の worktree teardown を専用 worker へ逃がし、IPC を即応答にする
status: todo
priority: high
labels: [v2, daemon, lifecycle, ipc, mcp, tui, ux]
dependson: []
related: [540]
created_at: 2026-07-24T22:40:07.703037+00:00
updated_at: 2026-07-24T22:40:07.703037+00:00
---

## 問題・根拠（コード調査で確定）

`session_remove` の重い worktree 削除が、いまも **IPC 接続の応答経路の内側**で同期実行される。

`perform_remove`（`crates/daemon/src/usecase/session_runtime.rs`）は [#1270](https://github.com/KKyosuke/usagi/pull/1270) で 3 段に分割された。

| 段 | 内容 | session lock |
|---|---|---|
| `begin_remove` | 検証、`Deleting` へ遷移、`DeletePlan` を durable に記録 | 保持する |
| `execute_remove` | `remove_session_tree` → nested worktree の `git worktree remove` → `fs::remove_dir_all(session_root)` | **解放済み** |
| `finish_remove` | 結果を durable に確定 | 再取得する |

lock は確かに解放されている。しかし 3 段はすべて **1 本の IPC request handler の中**で直列に走る（`start_ipc_accept_loop` が接続ごとに立てた `usagi-ipc-client` thread 上の `dispatch_session` → `perform_remove`）。したがって次が成立する。

- **呼び出した client の接続は削除完了まで応答を受け取れない**。`ClientPolicy`（`crates/core/src/usecase/client.rs`）の deadline budget は TUI 2,000ms / CLI 10,000ms / MCP 30,000ms である。coverage 実行後の session の `target/llvm-cov-target` は数 GB になるため、削除は分オーダーで、**どの client も deadline 内に応答を得られない**。MCP の `session_remove` は 30 秒で必ず timeout する。
- TUI の main lane は 1 接続を共有するため、その lane に並んだ session list / status refresh も削除の間ずっと待つ。terminal 描画は [#1272](https://github.com/KKyosuke/usagi/pull/1272) で専用接続へ分離済みなので固まらないが、sidebar 系は固まる。
- **中断すると再開されない**。`reconcile()`（daemon 起動時）は `Deleting` を `ReconcileInterrupted { stage: FailureStage::Delete }` で **`Failed` に落とす**。`DeletePlan` は durable に残っているのに削除は再開されず、半分消えた worktree tree と、session 名を所有し続ける record が残る。これは v1 で `git_teardown` 中断が次の手動 remove まで詰まった病理と同型である。
- daemon を持たない worker が無いため、削除中の daemon 停止・crash は必ずこの状態を作る。

補足: 残留 daemon が session worktree 内に cwd を持っていると `remove_dir_all` 自体が失敗・停滞する。その原因は [#540](540-fix-daemon-daemon-serve-self-shutdown-test-fixture-workspace.md) 側で断つ。

## 設計判断

正本は [document/proposals/13-daemon-singleton-and-teardown.md](../../document/proposals/13-daemon-singleton-and-teardown.md)。

**remove を「即時 accept + daemon 所有の teardown worker」へ変える。**

- `begin_remove` までを IPC handler で実行し、**その時点で応答を返す**。reply は既存の `SessionReply { operation_id, revision, body }` で表現できるため wire schema の変更は不要である（`body` は自由な `Value`）。
- teardown は **専用 worker thread 1 本**が直列に drain する（`spawn_pr_refresh_worker` と同型の worker）。worker は `remove_session_tree` を実行し、そのあと短時間だけ session lock を取って `finish_remove` を書く。直列なので N 件の削除が I/O を飽和させない。queue 深さは session 数で自然に有界である。
- **queue は新設せず、durable state から導出する**。`lifecycle == Deleting` かつ `delete_plan` を持つ record が、そのまま未完了 teardown の集合である。追加の永続 file を持たない。
- **`reconcile()` の `Deleting` の扱いを「`Failed` へ落とす」から「worker へ再投入する（resume）」へ変える**。`remove_session_tree` は `NotFound → Ok` で冪等なので、途中まで削除された tree に対して安全に再実行できる。これが本 issue の中心的な correctness 改善である。`Creating` / `Initializing` の `Failed` 化は変えない（create は effect を巻き戻せないため）。
- **client 表示は既存の投影で足りる**。`snapshot()` は [#529](529-fix-daemon-failed-session-remove.md) 以降すべての durable record を `lifecycle` 付きで投影するため、`Deleting` 行はすでに client から見える。`04-ipc.md` も `deleting` scope への launch を typed refusal として定義済みである。よって TUI / CLI / MCP は「即座に `deleting` 行になり、完了時に消える（失敗時は `Failed` になる）」を表示するだけでよい。
- **冪等性**: 同一 `operation_id` の再送は既存の journal replay で処理される。`Deleting` な session に対する**新しい** `operation_id` の remove は、重複投入せず進行中 operation を返す。

## やること

- teardown worker（1 thread・直列 drain）を daemon 合成ルートに追加し、`begin_remove` が生成した pending teardown を渡す。
- `session_remove` の IPC 応答を `begin_remove` 完了時点に前倒しし、`operation_id` と `deleting` を含む accepted 応答にする。
- `reconcile()` の `Deleting` を resume（worker 再投入）へ変更する。`Creating` / `Initializing` は現状維持。
- `Deleting` に対する新規 operation の扱い（進行中 operation を返す）を確定する。
- teardown 失敗を `Failed` + 診断可能な `failure` として確定する（削除できなかった原因を残す）。
- TUI / CLI / MCP の remove 完了待ちの表現を、accepted → `deleting` 行 → 消滅 に合わせる。MCP の `session_remove` は「受理」を返す契約になるため [document/07-mcp.md](../../document/07-mcp.md) を更新する。

## 受入条件

- 数 GB の `target/` を持つ session を削除しても、`session_remove` の応答が client deadline（TUI 2s / CLI 10s / MCP 30s）内に返る。削除中も他の IPC request（session list、terminal、agent）が即応答し続ける。
- 削除中に daemon を停止・crash させて再起動すると、teardown が **再開**され最終的に完了する（`Failed` に落ちない）。
- 削除失敗時は `Failed` + 原因が client から見える。同名 session の再作成は失敗 record の remove 後に可能である。
- 同一 session への remove の重複投入が worktree effect を二重実行しない。
- カバレッジ 100% を維持する。worker は port 注入で fake test 可能にし、実 IO の薄いラッパだけを合成ルートに置く。
- [document/05-daemon.md](../../document/05-daemon.md)（durable operation / session tree）と [document/07-mcp.md](../../document/07-mcp.md) を実装に合わせて更新する。
