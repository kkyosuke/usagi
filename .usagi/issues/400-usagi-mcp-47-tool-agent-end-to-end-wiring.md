---
number: 400
title: usagi mcp: 公開 47 tool を agent から最終処理まで接続する（end-to-end wiring・親)
status: in-progress
priority: high
labels: [mcp, epic]
dependson: []
related: []
created_at: 2026-07-20T04:52:42.569498+00:00
updated_at: 2026-07-20T21:20:53.003572+00:00
---

## 背景と問題

`usagi mcp`（agent 入口面）は `tools/list` で **47 tool** を公開するが、その大半が「agent の tool 呼び出し → MCP stdio → daemon/core → 永続化/agent 配送 → MCP 応答」の最終処理まで接続されていない。特に危険なのは、**実処理をしていないのに成功（`ResponseOutcome::Ok`）を返し、リクエストをそのままエコーする false-success no-op** で、agent は「worker を dispatch した」「supervisor を起動した」と誤認する。

本 issue はトリアージの正本（親）であり、47 tool の分類・根拠・修正方針・移行順序・受け入れ条件を定める。系統別の実装は子 issue（#401–#407）に分割する。

## 調査方法（根拠の担保）

現行 `main`（本ブランチは `main` から分岐、クリーン）のソースと、**本ブランチで新規ビルドした `target/debug/usagi`（v2.6.0）** の stdio 実挙動の双方で再確認した。加えて稼働中 daemon の live MCP も観測した。

- **重要（バイナリ差異）**: `session_status` を現行 main バイナリの `usagi mcp` に投げると `-32603 tool not yet implemented` を返すが、**稼働中 daemon の live MCP は実データを返す**。つまり稼働 daemon は main HEAD より先行したビルドである。分類はすべて**現行 main のソース＋新規ビルドバイナリ**を正とする。実装時は稼働中の別ブランチ作業と重複しないよう突き合わせること。

実挙動の実測（新規ビルド `usagi mcp` stdio、`tools/call`）:

| tool | 実測結果 | 判定 |
|---|---|---|
| `issue_create` / `memory_save` / `session_status` | `-32603 tool not yet implemented: <name>` | 未実装（正直なエラー） |
| `supervisor_start` | `Ok` で `{"action":"start","kind":"supervisor_tool","operation_id":…,"payload":{}}` をエコー | **false-success no-op** |
| `agent_list` | `Ok` で `{"action":"agent_list","kind":"dispatch_tool",…}` をエコー | **false-success no-op** |
| `session_prompt` | `-32603 InvalidArgument: invalid session request` | daemon 到達→`InvalidRequest` |

`tools/list` は 47 件（`crates/cli/src/mcp/tools/mod.rs:39` の assert と一致、serverInfo.version=2.6.0）。

## 47 tool の分類と根拠

### A. 完全実装（実 durable 効果あり）— 3

`session_create` / `session_remove` / `session_recover_legacy`。`serve.rs:338` の `session_action` で `DaemonRequest::Session`（kind `session`）へ routing → `dispatch_session`（`src/runtime/daemon.rs:1435`）→ `SessionRuntime`（`crates/daemon/src/usecase/session_runtime.rs`）で git worktree 生成/削除まで実行。

### B. false-success no-op（実処理なしで Ok エコー）— 13 【最優先で解消 → #401】

- **DispatchTool 非 decision アクション（7）**: `session_dispatch` / `session_get` / `agent_list` / `agent_get` / `agent_complete` / `agent_fail` / `agent_inbox`。`serve.rs:349` の `dispatch_tool_action` で `DaemonRequest::DispatchTool`（kind `dispatch_tool`）へ → router は `dispatch_user_decision`（`daemon.rs:1055`）に渡すが、同 handler は `UserDecision*` 以外を `daemon.rs:1174-1184` で `ipc::dispatch()` に丸投げ → `crates/daemon/src/presentation/ipc.rs:89-100` は kind が `{session,agent,dispatch}` 以外だと `ResponseOutcome::Ok` で **body をそのままエコー**（`ipc.rs:101-109`）。
- **supervisor_*（6）**: `supervisor_start/get/list/cancel/resolve_escalation/events`。`serve.rs:284` で `DaemonRequest::SupervisorTool`（kind `supervisor_tool`）へ → **router に該当 arm が無い**（`daemon.rs:1046-1056` の match は `_ => ipc::dispatch()`）→ 同じくエコー Ok。`SupervisorRuntime`（`crates/daemon/src/usecase/supervisor_runtime.rs`）は存在するが production composition に**一度も生成されていない**（`SupervisorRuntime::new` は test のみ）。

### C. 部分実装・last-mile 断（user_decision_*）— 6 【→ #406】

`user_decision_request/get/list/resolve/cancel/expire`。credential 付き caller からは `dispatch_user_decision`（`daemon.rs:1122`）が実 `UserDecisionStore` まで到達する。ただし:
- TUI の解決経路は `caller_context: None` を送る（`src/runtime/tui.rs:130-135`, `:162-167`）→ `daemon.rs:1208-1213` で `OwnershipUnknown` fail-closed。人間が回答できない。
- 解決した回答は outbox（`crates/core/src/infrastructure/store/user_decision.rs:111-116`）へ積まれるが、**consumer が production に存在しない**（`.events()` は test のみ）。元 agent へ返す last-mile が無い。

### D. daemon 到達→未実装エラー（`InvalidRequest`）— 1 【→ #403】

`session_prompt`。`serve.rs:344` で `DaemonRequest::Session`（Prompt）→ `SessionRuntime::handle` が `SessionAction::Setup | Prompt => Err(InvalidRequest)`（`session_runtime.rs:234-236`）。delegate 系が依存する肝。

### E. 未実装（正直な "tool not yet implemented" エラー）— 24 【issue/memory → #404、session → #403】

- **issue（6）**: `issue_create/get/to_prompt/search/update/delete`。docs は「store 系（cwd の `.usagi/issues/`）を core usecase で直接読み書き」と書くが（`document/07-mcp.md:54`）、実際は `Tool::call` 既定スタブ（`tool.rs:33-35`）のまま core usecase を一切呼ばない。
- **memory（4）**: `memory_save/get/search/delete`。同上。
- **session の非 routing（14）**: `session_list` / `session_status` / `session_complete` / `session_pr` / `session_note_get` / `session_note_update` / `session_todo_list` / `session_todo_add` / `session_todo_update` / `session_todo_remove` / `session_decision_list` / `session_decision_log` / `session_delegate_issue` / `session_delegate_brief`。`serve.rs:338-346` の `session_action` に載っておらず、`dispatch(name,…)` → `ToolError::Unimplemented`。なお `session_list` は `SessionRuntime::handle` が `List/Overview` を実装済みで **routing を足すだけ**の低コスト。

> 集計: A3 + B13 + C6 + D1 + E24 = **47**。全 tool の `Tool::call` は既定の `Unimplemented` スタブで、override は 1 つも無い（`crates/cli/src/mcp/tool.rs:33-35`）。

## 根本原因（5点）

1. leaf の `Tool::call` が全て既定 `Unimplemented` スタブ。store 系（issue/memory）は core usecase を呼んでいない。
2. `DispatchTool`（kind `dispatch_tool`）は `dispatch_user_decision` に一括 routing され、`UserDecision*` 以外は既定 `ipc::dispatch()` の**エコー**へ落ちる。
3. `SupervisorTool`（kind `supervisor_tool`）は router に arm が無く既定エコーへ。`SupervisorRuntime` 未 compose。
4. 実 agent orchestration は `DaemonRequest::Dispatch`（kind `dispatch`, `dispatch_dispatch`）だが、**この variant を構築する client がコードベースに 1 つも無い**（`daemon.rs:1331` は受信側の分解のみ）。MCP `session_dispatch` は `DispatchTool::Dispatch` を送るため実 dispatch 経路に到達しない。
5. `user_decision` は credential 前提。TUI は無 credential、かつ解決回答の caller 配送が未実装。

## 修正方針

1. **false-success を最優先で撲滅**（#401）。実 durable 効果が無い応答で `Ok` を返さない。未実装 tool は明示エラーを返す安全弁を先に入れ、以後の実装で個別に本処理へ置換する。
2. **未実装 tool**: 明示エラーで返す（現状の issue/memory/session 非 routing は既に明示エラーなので維持しつつ、本実装で順次接続）。
3. **実装済み扱いにする tool**: 正しい最終処理を行い、durable な効果を反映した結果を返す。エコーや偽 Accepted を返さない。

## 移行順序・依存関係（子 issue）

```
親 #400
├─ #401 false-success 撲滅（no-op echo→明示エラー）          … P0, 依存なし・最初
├─ #402 agent orchestration end-to-end                        … depends #401
├─ #403 session 観測/prompt/delegate                           … depends #401
├─ #404 issue/memory store tools                                … depends #401
├─ #405 supervisor runtime 配線＋tools                          … depends #401
├─ #406 user_decision last-mile（TUI credential＋caller 配送）  … depends #401
└─ #407 production E2E harness ＋ docs 整合                      … depends #401（各系に追随）
```

| 子 | 系統 | 対象 tool 数 | priority |
|---|---|---|---|
| #401 | false-success 撲滅（安全弁） | 13 | high (P0) |
| #402 | agent orchestration | 7 | high |
| #403 | session 観測/prompt/delegate | 15 | high |
| #404 | issue/memory store | 10 | medium |
| #405 | supervisor | 6 | medium |
| #406 | user_decision last-mile | 6 | medium |
| #407 | E2E harness ＋ docs | — | medium |

- #401 は各系の本実装が入るまでの安全弁。本実装 PR は自系 tool について #401 のエラーを実処理へ置換する。
- #403（`session_prompt` backend）は delegate 系（`session_delegate_*`）の前提。
- #407 の docs は各系の最終挙動に追随（記載＝実装済みの規約）。E2E harness は共有基盤として早期着手可。

## 後方互換性・v1 との差

- v1（`v1/`、出荷物）は別実装。v2 の MCP 面はまだ「動作する」と保証していない段階なので、no-op→明示エラー化は利用者体験の後退ではなく**誤成功の除去**。破壊的変更として扱う必要は低いが、`tools/list` の契約（name/inputSchema）は維持する。
- 稼働中 daemon が main より先行している点に留意（重複実装回避）。

## docs 修正対象

- `document/07-mcp.md`: issue/memory を「実装済みの store 系」と読ませる記述（`:54-56`）、session を「daemon IPC で動作」と読ませる記述を、現状（未接続）と最終形に合わせて是正。
- `crates/cli/src/mcp/guides/orchestration.md`（resource `usagi://guides/orchestration`）: `session_dispatch`/`session_prompt`/`session_status`/`session_delegate_*`/`session_complete` を使う dispatch→observe→complete のワークフローを説明しているが、これらが未接続/no-op の間は agent を誤誘導する。規約「記載＝実装済み」に反するため、各系の接続に追随して更新（未接続の tool を手順に載せない）。

## 受け入れ条件（親・全系共通）

- [ ] 47 tool のいずれも **false-success（実効果なしの Ok/Accepted）を返さない**。未実装は明示エラー、実装済みは実 durable 効果を反映した結果を返す。
- [ ] **production E2E テスト**（#407 harness）: `usagi mcp` を実プロセスで起動し、stdio JSON-RPC → 実 daemon → 永続化/agent 配送 → MCP 応答を通し、durable effect（issue ファイル生成、session 生成、dispatch による worker 起動、agent_complete の caller inbox 到達、user_decision 解決の caller 返却）を固定する。initialize/daemon autostart だけの現行 E2E を超える。
- [ ] docs（`07-mcp.md`・orchestration guide）が最終挙動に一致（記載＝実装済み）。
- [ ] 各子 issue（#401–#407）の系統別完了条件を満たす。
