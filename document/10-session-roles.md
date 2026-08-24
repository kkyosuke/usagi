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
  - [`<tools>` fragment](#tools-fragment)
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
session = "manager"

[roles.director]
summary = "全体方針を決める"
scopes = ["root"]
instructions = "要求を分解し、session の結果を統合する。"
[roles.director.delegation]
enabled = true
child_roles = ["manager", "coder"]
max_depth = 2
max_concurrency = 4

[roles.coder]
summary = "実装と検証を行う"
scopes = ["session"]
instructions = "依頼された変更を実装し、リスクに応じたテストを実行する。"
[roles.coder.delegation]
enabled = false

[roles.manager]
summary = "大きいタスクを分解・統合する"
scopes = ["session"]
instructions = "タスクを Executor へ委譲し、各結果を検証・統合して直近 caller へ報告する。"
[roles.manager.delegation]
enabled = true
child_roles = ["coder"]
max_depth = 2
max_concurrency = 4
```

role は組織上の責務を prompt として与える。Director が小さいタスクを session role の Executor へ直接 dispatch
する場合は 2 層、大きいタスクを Manager role へ dispatch し、その Manager が Executor を dispatch する場合は
3 層になる。dispatch binding が実行ごとの親子関係を保持するため、完了報告は Executor → Manager → Director と
一段ずつ返る。`delegation` block を定義した role は daemon admission で `enabled`、`child_roles`、`max_depth`、
`max_concurrency` を検証し、prompt の自己申告には依存しない。block を持たない version-1 role は互換性のため従来動作を維持する。
durable supervisor run ではこれに加えて immutable な `ExecutionPolicy` が dispatch 総数・並列数・深さを制限する。

会社テンプレートでは、利用者がTUI/CLIから手動作成する新規sessionを調整役として扱うため、`defaults.session` は
`manager` とする。Director/Managerが実行者を委譲するときは `role = "coder"` を明示し、既定値に依存しない。
sidebar は各session名の横に `[manager]` / `[coder]` と階層インデントを常時表示し、Garden は `[role] parent › session` の nameplate と session 内の Agent を表すうさぎを表示する。Director drawerはrootのDirectorを含む親子ツリーを表示する。

`session_delegate_issue` のように worker launch を後で行う入口も、queued prompt に authenticated caller を保存する。
したがって launch 方法によらず同じ dispatch binding が作られ、worker の `session_complete` は直近の親 inbox だけへ届く。
子の inbox commit 後は、live な Manager には通知を送り、停止中なら通知を next-launch queue に永続化する。
いずれかの role に `delegation` block がある会社モードでは、credential のない `session_delegate_issue` を拒否する。
`max_concurrency` は実行中の子だけでなく未起動の delegated prompt も予約枠として数え、Agent の runtime/model を変更しても
同じ session の利用数と絶対深度を引き継ぐ。

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

本節が Agent launch の system prompt 合成の正本である。prompt は次の 3 fragment を、この順で一度だけ合成する。

```text
code-defined scope safety prompt
<tools>injected MCP server が登録する tool 系統</tools>
<role id="...">effective instructions</role>
```

| fragment | 決めるもの | 省略される条件 |
|---|---|---|
| scope safety prompt | root / session worktree の境界 | 省略しない |
| `<tools>` | 配線済み MCP server が公開する tool 系統 | MCP を配線しない launch |
| `<role>` | effective role instruction | role 未割り当て |

順序は層の可変性で決まる。scope safety prompt は role で置換できない。`<tools>` は launch 時点の環境の事実で、
role より前に置くため role instruction で絞り込める。scope safety prompt は tool 名を 1 つも書かないので、
tool 系統の可用性は `<tools>` だけが述べる。

Claude adapter は合成済み文字列を単一 `--append-system-prompt` 値、Codex / Sakana AI
adapter は単一 `developer_instructions=<TOML string>` 値として ephemeral provision に渡す。initial user prompt へ連結しない。

### `<tools>` fragment

`<tools>` は 1 系統 1 行で、有効な系統だけを列挙する。無効な系統は「無い」とも書かず行そのものを落とす。
tool 名と引数を列挙せず、`tools/list` のスキーマが正本であることと、手順が resource
`usagi://guides/orchestration` にあることだけを述べる（正本は [7. MCP サーバ#tool 面](07-mcp.md#tool-面)）。

各行は launch scope に依存せず真である文にする。行が述べるのは「どこで何が受理されるか」で、
「この agent が何をしてよいか」ではない。例えば issue の書き込みは session worktree でだけ受理されるので、
session では許可、root では拒否として同じ 1 行が両方で真になる。scope 別の variant を作らない。

| 行 | 条件 |
|---|---|
| `- session:` | MCP を配線する launch では常に載る（session 系統は無効化できない） |
| `- issue:` | effective `issue_enabled` |
| `- memory:` | effective `memory_enabled` |
| `- local_llm_ask:` | Global `local_llm.enabled` |

`issue_enabled` / `memory_enabled` の effective 値は、Global 設定に **daemon に登録された workspace root** の
`.usagi/settings.json` を重ねて解決する。設定を tool 系統へ写す規則は `usagi-core` の `domain::agent::mcp_tools` に
1 つだけあり、`usagi mcp` が registry を組むときも同じ関数を通る。prompt が述べる系統と `tools/list` が登録する
系統が同じ設定から出ることは、この共有によって構造的に保証する。session worktree は `.usagi/settings.json` を
持たない（git 追跡外）ため、worktree を権威にすると workspace の上書きが消える。`local_llm` は Global だけが権威である
（正本は [7. MCP サーバ#daemon Agent への local LLM 配線](07-mcp.md#daemon-agent-への-local-llm-配線)）。

設定が読めない場合は既定値へ倒さず launch を拒否し、error log に記録する。`usagi mcp` が serve loop の開始前に
失敗するのと同じ規則である。既定へ倒すと、自身の MCP server が登録できない tool 系統を prompt が述べた Agent が起動する。

## safe projection と非永続データ

`session_list` / `session_status` / `session_get` は `role_id` と current definition の `role_summary` を safe metadata として返す。list / status / overview の各 session は、これに加えて `parent_session_id` / `parent_session_name` / `organization_depth` / `organization_path` を返す。path は root の `Director` から当該 session までで、同一 session 内の Agent handoff は階層を増やさない。
catalog が読めない場合も lifecycle metadata を返し、summary は `null` になる。

TUI は `role_id` / `role_summary` と dispatch binding 由来の `parent_session_id` / `agent_status` を stable session identity keyed の controller projection として保持し、
legacy `SessionRecord` や `state.json` へコピーしない。sidebar は role ID だけを badge 表示し、role metadata を
attach / remove などの lifecycle capability 判定には使わない。

Create Session の inline form は effective catalog の session scope 候補を read-only に表示し、
`defaults.session` を初期選択する。↑↓ / Tab で候補を切り替え、submit は role ID だけを daemon へ送る。
catalog が不正な場合は picker を空に縮退させ、既存 session の lifecycle 操作を継続する。

Overview の `roles [workspace|global]` は対象 layer の versioned `roles.toml` を source のまま編集する。
保存時は effective two-layer catalog として検証し、error は draft を保持したまま inline 表示する。成功時は
source を再 serialize せず、コメント・順序・空白を維持して durable temp file + atomic rename で置換する。
editor の14行の表示窓は ↑ / ↓ で 1 行、PageUp / PageDown で 1 ページ移動する。読み込み時と末尾への
追記時は source の末尾へ自動追従する。

次の場所へ role instruction を保存しない。

- daemon wire response
- dispatch agent / run / binding store
- durable launch request / snapshot
- lifecycle log と error log

instruction は catalog から launch ごとに読み、adapter の ephemeral spawn arguments にだけ存在する。

## 互換性

`ManagedSession.role_id` は serde default 付き optional field である。旧 `sessions.json` は role 無しとして読み、current default を自動採用しない。
role 無し session は従来の generic session scope prompt を使う。role は sandbox/guard を変更せず、reviewer を選んでも filesystem 権限は変わらない。
