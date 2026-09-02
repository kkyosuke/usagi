# 14. session role（採用済み）

> [設計提案一覧](README.md) ｜ 実装済み仕様: [session role](../10-session-roles.md)

> **Status:** 採用済みの設計履歴
>
> **Baseline:** 原版 commit `fbbfb4fbf0fef0f60a01406ea42e3a5c3df12f76`（2026-08-01）。本文は session role を導入した時点の snapshot であり、現在仕様ではない。現行の prompt 合成と TUI 契約は [session role](../10-session-roles.md) を参照する。

本提案の catalog、stable assignment、daemon 再検証、prompt 合成、safe projection の契約は実装され、
[10. session role](../10-session-roles.md) へ畳み込まれた。現在の動作仕様は同書を正本とする。

## 採用した設計判断

- role は prompt policy であり、filesystem/MCP authorization ではない。
- role definition は live Agent を変えず、次回 launch / explicit resume から反映する。
- managed session の `role_id` は incarnation 作成時に固定し、途中変更しない。
- workspace catalog は target branch でなく daemon の registered workspace root を権威とする。
- 導入時点では scope safety prompt、effective role、optional local-LLM suffix の順に合成する案を採用した。現在の合成順序・構成要素は [現行仕様](../10-session-roles.md#prompt-合成) を参照する。
- role instruction は wire、dispatch store、durable launch snapshot、log に保存しない。

TUI の catalog editor、create picker、badge は、提案時点では後続実装として issue store で追跡していた。
これらを含む現在の表示・操作契約は [TUI 仕様](../03-tui.md) を参照する。
