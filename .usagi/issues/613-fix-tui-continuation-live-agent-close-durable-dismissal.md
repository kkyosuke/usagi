---
number: 613
title: fix(tui): continuation 未観測の live Agent close を durable dismissal にする
status: done
priority: medium
labels: [review, v2, tui, agent, lifecycle, correctness]
dependson: []
related: [506, 599, 600]
created_at: 2026-07-31T06:00:00+00:00
updated_at: 2026-08-01T01:21:46.596919+00:00
---

## Finding（P2 TUI）

#599 により continuation が未観測でも live chat は投影されるが、`crates/tui/src/presentation/mod.rs::close_focused_terminal_pane` は live tab の durable dismissal key を `WorkspaceUi::agent_continuation_for` からしか得ない。`None` なら `AgentTabIntentMutation::DismissAndSelect` を skip して runtime tabだけ閉じるため、次の inventory replay/reconnect で同じ conversation が再出現する。

## 最小修正方針

live terminal identity 自体で dismissal intent を永続化し、continuation が後で観測されたとき同じ intent へ reconcile するか、daemon inventory に stable dismissal fence を追加する。単なる local hide を durable close と表示しない。

## テストと受け入れ条件

- continuation `None` の live Agent を close し inventory を replay しても再出現しない。
- 後から continuation が判明しても dismissal と selection が一意に reconcile される。
- persistence failure 時は tab を閉じず safe feedback を表示する既存 atomic UI 契約を維持する。
