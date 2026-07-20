---
number: 483
title: refactor(mcp): tool metadata・routing・authorization を単一 descriptor にする
status: todo
priority: medium
labels: [review, v2, mcp, architecture]
dependson: []
related: [60, 97, 120, 400, 407]
parent: 453
created_at: 2026-07-20T12:06:48.850492+00:00
updated_at: 2026-07-20T12:06:48.850492+00:00
---

## 問題・影響

root/v2 MCP は `crates/cli/src/mcp/tools::registry()` の name/description/schema と、`serve.rs::{tools_list_result,tools_call,session_action,dispatch_tool_action,supervisor_tool_action}` の routing/caller policy を別々に列挙する。advertise した tool に route がない、実 route と schema/auth がずれる、といった false-success/unauthorized regression を型で防げない。

## 成立条件 / 再現フロー

registry に tool を追加・rename して routing match の更新を省く、または params/auth policy を片側だけ変更する。`tools/list` は成功するが call は unimplemented/誤 route になり、compile/test が必ずしも失敗しない。

## 対象責務と非対象

tool descriptor が metadata、input schema/validator、execution route、caller/provenance policy を所有する SSoT 化を対象とする。各 tool の business logic、SupervisorRuntime #405、decision last-mile #406、JSON-RPC envelope #482 は非対象。

## 受入条件

- [ ] 全 advertised tool が exactly one の executable route と authorization policy を同じ descriptor から得る。
- [ ] unadvertised route、duplicate name/route、schemaとruntime validatorの不一致を startup/compile/test で拒否する。
- [ ] session/dispatch/supervisor/store の追加が中央 match の多重更新を要求しない。
- [ ] unimplemented tool は false success ではなく descriptor の明示 capability/error を返す。

## 必須回帰テスト

47 tool 全件について advertised↔route の全単射、schema-valid/invalid params、caller policy、duplicate/unadvertised fixture を自動列挙して検証する。

## docs / 移行影響

`document/07-mcp.md` と開発者向け tool 追加手順を descriptor 起点に更新する。外部 tool name/schema は維持し、wire migration は不要。
