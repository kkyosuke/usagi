---
number: 611
title: fix(mcp): session_delegate_brief の session 作成と dispatch を失敗時 atomic にする
status: todo
priority: high
labels: [review, v2, mcp, daemon, session, agent, correctness, lifecycle]
dependson: []
related: [502, 546, 547]
created_at: 2026-07-31T15:00:00+09:00
updated_at: 2026-07-31T15:00:00+09:00
---

## Finding（P1 partial side effect）

`src/runtime/daemon.rs::dispatch_session` の `SessionAction::DelegateBrief` は selector/caller を一部検証した後、session/worktree の create/commit を完了してから `AgentRuntime::dispatch` する。dispatch admission、ownership、profile/model、PATH/readiness、store、PTY spawn のどれかが失敗すると error reply なのに session/worktree が残る。さらに `crates/cli/src/mcp/tools/session.rs::SessionDelegateBrief::input_schema` が公開する `agent.id` branch は、作成直後 session への existing Agent ownership/scope check と整合せず拒否され得る。

## 最小修正方針

公開 selector の全 semantic validation と dispatch reservation を create 前に完了し、create+dispatch を durable saga として journal する。dispatch が commit できなければ compensating teardown を必ず記録・完遂し、再起動後も回収する。成立しない `agent.id` branch は削除するか、明確な ownership semantics を実装する。

## テストと受け入れ条件

- invalid existing id、ownership mismatch、unknown model、missing executable、store/spawn failure の各 fault injection 後に Available/orphan session、worktree、branch、pending launch が残らない。
- crash を create 後/dispatch 前に入れて restart すると compensation または dispatch が一意に完了する。
- success reply のときだけ session と admitted run がともに存在し、同一 operation retry は二重作成しない。
