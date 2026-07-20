---
number: 482
title: fix(mcp): JSON-RPC 2.0 validation と protocol negotiation を実装する
status: done
priority: medium
labels: [review, v2, mcp, protocol]
dependson: []
related: [400, 407]
parent: 453
created_at: 2026-07-20T12:06:48.514964+00:00
updated_at: 2026-07-20T23:22:35.434494+00:00
---

## 問題・影響

root/v2 の `crates/cli/src/mcp/serve.rs::handle_line_with_client` は top-level object、`jsonrpc: "2.0"`、id 型、method、params を厳格に検証せず、`initialize_result` は client の任意 `protocolVersion` を echo する。initialize 前 tool call や unsupported version を受理し、invalid notification が effect を起こす可能性がある。

## 成立条件 / 再現フロー

array/scalar、欠落/異なる `jsonrpc`、不正 id/params、unsupported protocolVersion、initialize 前 `tools/call`、method 欠落 notification を stdio に送る。JSON-RPC/MCP 規定の error/lifecycle ではなく silent ignore、echo、tool execution が起こる。

## 対象責務と非対象

JSON-RPC 2.0 request/notification validation、MCP initialize negotiation と server lifecycle gating を対象とする。個別 tool routing SSoT は #483、tool 実装内容は #400 系、transport encryption は非対象。

## 受入条件

- [ ] parse error、invalid request、method not found、invalid params を規定 code/id semantics で返す。
- [ ] notification は response を返さず、invalid/未初期化 notification が durable effect を起こさない。
- [ ] server supported version を明示し、unsupported version は echo せず negotiation error にする。
- [ ] initialize→initialized→tool/resource の状態遷移と duplicate/out-of-order handling を定義する。

## 必須回帰テスト

raw stdio fixture で object/array/batch扱い、jsonrpc/id/method/params 全 invalid case、notification、version matrix、initialize lifecycle、effect count 0/1 を検証する。

## docs / 移行影響

`document/07-mcp.md` に対応 protocol version と lifecycle/error contract を追記する。互換性のない client には silent acceptance ではなく明示 error を返す。
