---
number: 329
title: feat(daemon): agent の user decision request を durable state と inbox 配送へ接続する
status: todo
priority: high
labels: [daemon, mcp, cli, supervisor, durable]
dependson: [323, 326, 327]
related: [328, 330, 219, 268, 271]
parent: 324
created_at: 2026-07-18T00:00:00+00:00
updated_at: 2026-07-18T00:00:00+00:00
---

## 目的

supervisor 配下の agent が `user_decision_request` で人間の選択を要求し、接続を block せず
`decision_id` と `waiting_for_user` を返せるようにする。質問と回答は durable に残し、解決時には
待機 run/agent の durable inbox へ結果 event を exactly-once で配送する。設計の正本は
[document/proposals/09-user-decision-mcp.md](../../document/proposals/09-user-decision-mcp.md) である。

## やること

- daemon state に `UserDecision` を追加する。immutable な workspace/session/agent/run/caller provenance、
  title、prompt、stable `option { id, label, description? }`、freeform 許可、任意期限、idempotency key、
  state（pending/resolved/cancelled/expired）と回答を atomic write + lock で保存する。
- MCP `user_decision_request { title, prompt, options, allow_freeform?, expires_at?, idempotency_key? }`
  を追加する。所有者は #323 の実行コンテキストと #325/#326 の provenance から解決し、agent supplied
  workspace/run/agent/caller は受け付けない。request は入力待ちせず即時に
  `{ decision_id, status: "waiting_for_user" }` を返す。
- request 成功時に owner task/run を AwaitingDecision/WaitingForUser に遷移し、policy が解決前の新規
  dispatch/effect を拒否するよう #327 の admission と接続する。agent が request 後に継続作業できる
  状態へ戻さない。
- daemon control API で workspace-scoped pending list、get、resolve、cancel、expire を提供する。
  `user_decision_get` は recovery/debug 用に限り、agent polling を主経路にしない。
- resolve は option ID または許可された freeform だけを受け、decision の compare-and-set と
  `UserDecisionResolved` inbox event append を一つの durable commit protocol で一度だけ行う。recipient は
  保存済み provenance から導く。cancel、期限、親 run 終了は terminal 化し、resume event を出さない。
- supervisor loop を本 issue で実装済みと文書化しない。#326 の loop が inbox event を今後消費して resume
  する接続点だけを実装し、実際の再開範囲は既存 scheduler task と整合させて明記する。

## 受け入れ条件

- restart 後に pending/terminal state と回答を復元でき、同じ idempotency key の retry は同一 ID を返す。
- 不正 option、freeform 非許可、二重回答、foreign workspace/decision、cancel/expire 後の回答は typed safe
  error で、state と inbox を変更しない。
- resolve 成功では durable decision と inbox event が片方だけにならず、duplicate/restart でも二重配送しない。
- MCP contract、durable store、daemon IPC、run wait transition、cancel/expire/parent exit、権限、restart、
  duplicate を deterministic test で固定し coverage 100% を維持する。

## 非目標

- decision modal/pending list の TUI（#330）。
- 未実装の supervisor consumer が agent process を自動再開したという主張。

## テスト方針

- `cargo test -p usagi-core`
- `cargo test -p usagi-daemon`
- `cargo test -p usagi-cli mcp`
- push/PR 前は [品質チェック](../../document/06-conventions.md#品質チェックリスク比例の-gate)の full gate。
