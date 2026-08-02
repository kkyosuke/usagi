# 10. session role

> [ドキュメント目次](README.md) ｜ ← 前へ [9. 環境変数設定](09-env.md)

session role の設定、割り当て、daemon 検証、Agent prompt 合成の仕様正本である。role は作業方針を表す prompt policy であり、
filesystem sandbox、MCP authorization、session lifecycle の権限ではない。

## 目次

- [モデル](#モデル)
- [catalog](#catalog)
- [割り当てと入口](#割り当てと入口)
- [daemon 検証](#daemon-検証)
- [prompt 合成](#prompt-合成)
- [safe projection と非永続データ](#safe-projection-と非永続データ)
- [互換性](#互換性)

## モデル

| 概念 | 型・保存先 | 変更契約 |
|---|---|---|
| scope | `RoleScope`（`root` / `session`） | launch identity から決まり、role では変更しない |
| assignment | `ManagedSession.role_id: RoleId?` | managed session incarnation の作成時に固定する |
| definition | global / workspace `roles.toml` | live Agent は変えず、次回 launch / explicit resume から反映する |

`RoleId` は 1–64 byte の小文字 ASCII kebab-case である。session 名から role を推測せず、session の途中で `role_id` を変更しない。
role を変える場合は別 session を作成する。

## catalog

| layer | path | precedence |
|---|---|---|
| global | `<data-dir>/roles.toml` | fallback |
| workspace | `<registered-workspace-root>/.usagi/roles.toml` | global を上書き |

両ファイルは `version = 1` を持つ。workspace の同一 role ID は global 定義を field 単位で混ぜず、定義全体を置換する。
default は `workspace → global → 未指定` の順で解決する。両ファイルが無い場合は role 無しの互換モードとなる。

```toml
version = 1

[defaults]
root = "director"
session = "coder"

[roles.director]
summary = "全体方針を決める"
scopes = ["root"]
instructions = "要求を分解し、session の結果を統合する。"

[roles.coder]
summary = "実装と検証を行う"
scopes = ["session"]
instructions = "依頼された変更を実装し、リスクに応じたテストを実行する。"
```

reader は future version、不正な role ID、空の `scopes`、未知 scope、16 KiB を超える instruction、NUL、対応 scope を許可しない
default を拒否する。workspace catalog の権威は target session branch ではなく daemon に登録された workspace root である。

## 割り当てと入口

CLI は optional `--role` を受け取る。

```text
usagi session create review-auth --role reviewer
```

MCP の `session_create` / `session_delegate_issue` / `session_delegate_brief` は top-level `role`、`session_dispatch` は
`session.role` を受け取る。wire に送るのは role ID だけで、instruction は送らない。

| 対象 | selector 省略 | selector 指定 |
|---|---|---|
| 新規 managed session | effective `defaults.session` | session scope を許可する role |
| 既存 managed session | 保存済み assignment を維持 | 同一 role は冪等、不一致は conflict |
| legacy session | role 無しを維持 | 不一致として拒否 |

## daemon 検証

daemon は create / dispatch / delegate の reservation 直前に catalog を再読し、role ID、scope、default を fail-closed で検証する。
MCP server 起動時の schema snapshot や client の cwd を policy authority にしない。malformed catalog でも list / get / status / remove は
継続できるが、新しい reservation と launch は拒否する。

launch / explicit resume では保存済み `role_id` に対して current catalog を再読する。definition の編集は実行中 Agent を変更せず、次の
process launch だけに反映される。

## prompt 合成

prompt は次の順で一度だけ合成する。

```text
code-defined scope safety prompt
<role id="...">effective instructions</role>
code-defined optional local-LLM suffix
```

scope safety prompt は role で置換できない。Claude adapter は合成済み文字列を単一 `--append-system-prompt` 値、Codex / Sakana AI
adapter は単一 `developer_instructions=<TOML string>` 値として ephemeral provision に渡す。initial user prompt へ連結しない。

## safe projection と非永続データ

`session_list` / `session_status` / `session_get` は `role_id` と current definition の `role_summary` を safe metadata として返す。
catalog が読めない場合も lifecycle metadata を返し、summary は `null` になる。

次の場所へ role instruction を保存しない。

- daemon wire response
- dispatch agent / run / binding store
- durable launch request / snapshot
- lifecycle log と error log

instruction は catalog から launch ごとに読み、adapter の ephemeral spawn arguments にだけ存在する。

## 互換性

`ManagedSession.role_id` は serde default 付き optional field である。旧 `sessions.json` は role 無しとして読み、current default を自動採用しない。
role 無し session は従来の generic session scope prompt を使う。role は sandbox/guard を変更せず、reviewer を選んでも filesystem 権限は変わらない。
