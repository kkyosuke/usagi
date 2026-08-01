# 14. session role（採用済み）

> [設計提案一覧](README.md) ｜ 実装済み仕様: [session role](../10-session-roles.md)

本提案の catalog、stable assignment、daemon 再検証、prompt 合成、safe projection の契約は実装され、
[10. session role](../10-session-roles.md) へ畳み込まれた。現在の動作仕様は同書を正本とする。

## 採用した設計判断

- role は prompt policy であり、filesystem/MCP authorization ではない。
- role definition は live Agent を変えず、次回 launch / explicit resume から反映する。
- managed session の `role_id` は incarnation 作成時に固定し、途中変更しない。
- workspace catalog は target branch でなく daemon の registered workspace root を権威とする。
- scope safety prompt、effective role、optional local-LLM suffix の順に一度だけ合成し、product adapter へ ephemeral に渡す。
- role instruction は wire、dispatch store、durable launch snapshot、log に保存しない。

TUI の catalog editor、create picker、badge は daemon projection を UI-only state として運ぶ必要があり、永続 `SessionRecord` へ
assignment を複製しない独立実装として issue store で追跡する。
