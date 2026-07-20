# 提案: supervisor の user decision request と durable な回答配送

> [設計提案の目次](README.md) ｜ [ドキュメント目次](../README.md) ｜ ← 前へ [agent dispatch MCP](08-agent-dispatch-mcp.md)

agent が人間の判断を要求する MCP、durable decision、TUI 回答の設計記録。正本の実行契約は
[MCP サーバ](../07-mcp.md) に置く。#329/#330 は domain 型・store・TUI reducer skeleton、#378 は
MCP/daemon の durable store 接続、#379 は TUI の本番接続・自動表示、#383 は daemon-managed agent の
caller provenance、#406 は workspace-scoped な TUI 回答面と caller の durable 回答取得を実装した。

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

option `id` は stable で一意な machine key とする。request は人間入力を待たず即時に返る。通常フローでは
`user_decision_get` を呼ばない。get は再接続、障害復旧、デバッグで durable record を調べるためだけに使う。

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

resolve は `Resolved` の durable 記録と `UserDecisionResolved` outbox event append を一度だけ commit する。
event recipient は保存済み owner から導く。consumer は event と decision の owner・回答を照合し、元の run が
なお Running で binding が一致するときだけ回答を continuation prompt として配送してから outbox を確認済みにする。
stale または終了済み run の event は配送せず破棄する。cancel/expire は delivery event を作らない。

```text
agent request -> daemon: Pending + waiting -> immediate MCP response
TUI resolve  -> daemon: Resolved + durable delivery event
daemon       -> original running run: continuation prompt with answer
```

## TUI

TUI は workspace-scoped pending decision を modal と一覧で表示する。MCP request により新着の pending decision が
届くと、その decision の回答 modal を直接開く。modal は title、prompt、option、期限、許可された場合だけ freeform
editor を表示する。dismiss は modal だけを閉じ、一覧から再表示できる。回答は daemon confirmation event を受けて
から modal を閉じる。失敗時は editor と error を保ち、再試行できる。reconnect/resync では pending 一覧を復元する。

## 実装状況と未接続点（triage）

`user_decision_get` が成功を返すのに回答 modal が出ない不具合の triage 結果と、その後の接続状況。
#378 と #379 により durable store、TUI 本番 port、pending modal は接続済みである。一方、daemon-managed
Codex が注入する MCP process には caller provenance が無く、`user_decision_request` が owner を解決できない
ため fail-closed になる。この修正は #383 で追跡する。

### 層ごとの gap

| 層 | 現状 | 接続点 |
|---|---|---|
| MCP → daemon（core/cli） | `user_decision_*` は `DispatchToolAction::UserDecision*` へ map される | 実装済み（#378） |
| daemon / 合成ルート dispatch | durable store へ到達するが、owner を workspace 全体の Running dispatch から推測している | Codex runtime-scoped caller context（#383） |
| daemon → TUI 配送 | decision snapshot / resolve の daemon-backed port と reducer projection を使う | 実装済み（#379） |
| TUI 本番 port・自動表示 | decision command port と pending 到着時の modal open を実行する | 実装済み（#379） |
| 解決回答 → caller | outbox consumer が durable decision と照合して配送済みにし、credential 付き caller が `user_decision_get` で取得する | 実装済み（#406） |

### 修正方針（層順）

```text
[383] Codex MCP caller provenance（378/379 と関連）
  daemon が runtime-fenced、opaque な MCP caller context を発行し、Codex の注入 MCP child だけへ渡す
  MCP serve は context を forward し、daemon が active runtime / scope / generation / terminal incarnation と照合する
  root・session・nested session、reconnect、stale/recreated session、context 無し・forged caller を統合 test で固定する
```

安全性の不変条件（未許可 freeform を送らない、空回答・不正 option を送らない、dismiss は daemon state を変えない、
resolve confirmation でのみ一覧から除く、reconnect で pending を復元、stale/duplicate response を安全に収束）は
既に reducer test で固定されており、接続作業はこれらを回帰させないこと。
