# 提案: supervisor の user decision request と durable な回答配送

> [設計提案の目次](README.md) ｜ [ドキュメント目次](../README.md) ｜ ← 前へ [agent dispatch MCP](08-agent-dispatch-mcp.md)

agent が人間の判断を要求する MCP、durable decision、TUI 回答の設計提案。未実装のため正本仕様には載せず、
実装 task は #329–#330 で追跡する。#329/#330 は domain 型・store・TUI reducer skeleton までを入れたが、
MCP/daemon の durable store 接続は #378 で実装され、TUI の本番接続・自動表示は #382 で追跡する。

## 目次

- [契約](#契約)
- [所有者と状態](#所有者と状態)
- [配送と再開の境界](#配送と再開の境界)
- [TUI](#tui)
- [実装状況と未接続点（triage）](#実装状況と未接続点triage)

## 契約

```
user_decision_request {
  title: string,
  prompt: string,
  options: [{ id: string, label: string, description?: string }],
  allow_freeform?: boolean,
  expires_at?: string,
  idempotency_key?: string
} -> { decision_id: string, status: "waiting_for_user" }
```

option `id` は stable で一意な machine key とする。request は人間入力を待たず即時に返る。状態取得が必要な
`user_decision_get` は recovery/debug 専用であり、agent polling は回答を渡す主経路ではない。

## 所有者と状態

decision の workspace/session/agent/run/caller は MCP 実行コンテキストと durable provenance から解決する。
payload に owner を含めないため、agent は宛先を偽装できない。質問、選択肢、期限、状態、回答は daemon state
に atomic write と lock で保存する。

```text
Pending --resolve--> Resolved
   |                 
   +--cancel-------> Cancelled
   +--deadline-----> Expired
   +--parent end---> Cancelled
```

resolve は Pending の compare-and-set である。不正 option、freeform 非許可、二重回答、terminal state は
変更しない。同じ owner と idempotency key の再送は同じ decision を返し、異なる内容での key 再利用は error にする。
request 成功時は owner task/run を waiting state にし、解決前の新しい effect を許可しない。

## 配送と再開の境界

resolve は `Resolved` の durable 記録と `UserDecisionResolved` inbox event append を一度だけ commit する。
event recipient は保存済み owner から導く。cancel/expire は resume event を配送しない。

```text
agent request -> daemon: Pending + waiting -> immediate MCP response
TUI resolve  -> daemon: Resolved + durable inbox event
supervisor   -> future inbox-event consumer that resumes work
```

最後の矢印は未実装である。この提案は安全な記録と配送を対象とし、自動 resume を実装済みと主張しない。

## TUI

TUI は workspace-scoped pending decision を modal と一覧で表示する。modal は title、prompt、option、期限、
許可された場合だけ freeform editor を表示する。dismiss は modal だけを閉じ、一覧から再表示できる。回答は
daemon confirmation event を受けてから UI から除き、reconnect/resync では pending 一覧を復元する。

## 実装状況と未接続点（triage）

`user_decision_get` が「dispatch 成功・回答待ち」「テスト用 decision 作成済み」と返すのに回答 modal が出ない
不具合の triage 結果。原因は単一の bug ではなく、この機能が**本番の 3 層すべてで未接続**であることにある。
`user_decision_request` は daemon の echo stub に落ちて何も永続化されず、TUI へ push も届かず、届いても modal は
自動で開かない。false success の text だけが agent に返る。

### 層ごとの gap

| 層 | 現状 | 接続点 |
|---|---|---|
| MCP → daemon（core/cli） | `user_decision_request`/`get`/`list`/`resolve`/`cancel`/`expire` は `DispatchToolAction::UserDecision*` へ map される | MCP tool registry と `dispatch_tool_action` |
| daemon / 合成ルート dispatch | `dispatch_tool` は `UserDecisionStore` の request/get/list/resolve/cancel/expire へ到達する。owner は payload ではなく唯一の running dispatch binding から復元し、曖昧な provenance は fail-closed にする | 合成ルートの `dispatch_tool` handler |
| daemon → TUI 配送 | `DaemonPush::DecisionsSnapshot`/`DecisionResolved`/`DecisionError` と reducer への adapter は TUI 側に存在するが、これを wire から構築する decoder も、daemon がこの push を送る経路も無い | daemon の pending 投影 push（または snapshot 応答）と TUI transport decoder |
| TUI 本番 port 注入 | `run_workspace_controller` は decision port を引数に取らず、`DaemonBackend` は本番で構築されず（`DaemonBackend::new` は test のみ）、`DecisionPort` は常に `NoDecisions`（no-op）。`Effect::RefreshDecisions`/`ResolveDecision` は本番で捨てられる | `run_workspace_controller` へ `DecisionPort` を追加し合成ルートで daemon-backed 実装を注入 |
| TUI reducer 自動表示 | `BackendEvent::Decisions` は `state.decisions`（一覧）だけを更新し、`reconcile_decision_overlay` は**既に開いている** overlay しか調整しない。pending が増えても `Overlay::Decisions` を開かない。手動の `AppKey::OpenDecisions` も key binding が無く（`app_event_from_key` 等に未登録）到達不能 | pending 到着で overlay を自動 open する reducer 分岐＋手動 open の key binding |

### 修正方針（層順）

```text
[382] TUI 本番接続＋自動表示（378 に依存）
  run_workspace_controller に DecisionPort を追加、合成ルートで daemon-backed 実装を注入
  transport decoder が Decisions* push を DaemonPush へ decode
  reducer: pending 到着で Overlay::Decisions を自動 open（既存 modal input ownership を尊重）
  手動 open の key binding を追加
  reducer/render/fake-daemon/runtime integration の regression test（reconnect/resync/stale/duplicate/dismiss）
```

安全性の不変条件（未許可 freeform を送らない、空回答・不正 option を送らない、dismiss は daemon state を変えない、
resolve confirmation でのみ一覧から除く、reconnect で pending を復元、stale/duplicate response を安全に収束）は
既に reducer test で固定されており、接続作業はこれらを回帰させないこと。
