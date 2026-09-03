# 10. session role

> [ドキュメント目次](README.md) ｜ ← 前へ [9. 環境変数設定](09-env.md) ｜ 次へ → [11. キーバインド](11-keybindings.md)

session role の設定、割り当て、daemon 検証、Agent prompt 合成の仕様正本である。role は作業方針を表す prompt policy であり、
filesystem sandbox、MCP authorization、session lifecycle の権限ではない。

## 目次

- [モデル](#モデル)
- [catalog](#catalog)
  - [`roles.toml` の設定例](#rolestoml-の設定例)
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
| template | global / workspace settings の `team_template` | Config 保存後の新規 launch / explicit resume から反映する |
| definition | global / workspace `roles.toml` | live Agent は変えず、次回 launch / explicit resume から反映する |

`RoleId` は 1–64 byte の小文字 ASCII kebab-case である。session 名から role を推測せず、session の途中で `role_id` を変更しない。
role を変える場合は別 session を作成する。
現在の Team と role catalog は workspace 単位であり、Work Run ごとの Team override / snapshot は持たない。
goal-driven の root Agent とそこから委譲する session も、launch 時点の同じ effective catalog を使う。

## catalog

Config の `Team` は次の組み込み catalog を選ぶ。global の値は workspace 登録時の初期値になり、Workspace Config の値が
その workspace で優先される。Team 行で `Enter` を押すと3種類を構造図付きカードで比較でき、狭い端末では同じ候補を
縦リストで表示する。`none` はカードとは別の `Use no template` actionで選択する。

| 表示 | `team_template` | root default | session default | 許可する委譲経路 | `max_depth` |
|---|---|---|---|---|---:|
| none | `none` | 未指定 | 未指定 | 組み込み role なし | — |
| hierarchical | `hierarchical` | Director | Manager | Director → Manager / Worker、Manager → Worker | 2 |
| flat | `flat` | Director | Worker | Director → Worker | 1 |
| pipeline | `pipeline` | Director | Planner | Director → Planner → Implementer → Tester | 3 |

`none` は既定値であり、組み込み catalog を注入しない。未知の `team_template` token、および読み取れないworkspace設定も
委譲権限を暗黙に増やさないよう `none` へ縮退する。
各テンプレートの委譲上限 `max_concurrency` は 4 である。パイプライン型は role と委譲経路によって工程順を制約し、
独立した workflow engine や自動ステージ遷移は追加しない。各 Agent が role instruction に従って次工程へ dispatch する。

catalog は次の順で合成する。後の layer にある同一 role ID は、前の定義を field 単位で混ぜず定義全体を置換する。
default は各 layer で指定された scope だけを上書きする。

| layer | path | precedence |
|---|---|---|
| built-in | settings の `team_template` | base |
| global | `<data-dir>/roles.toml` | built-in を上書き |
| workspace | `<registered-workspace-root>/.usagi/roles.toml` | global を上書き |

`roles.toml` は `version = 1` を持つ。選択したテンプレートを土台に差分定義を重ねられるため、テンプレート選択と
catalog 編集は両立する。`none` を選び両ファイルも無い場合は role を適用しない。

### `roles.toml` の設定例

組み込み Team をそのまま使う場合は `roles.toml` を作る必要はない。独自 role、既定 role、委譲制限を追加または
上書きする場合は、Overview で `roles workspace`（既定）または `roles global` を開いて次の形式を編集する。

```toml
version = 1

[defaults]
root = "director"
session = "manager"

[roles.director]
summary = "全体方針と結果統合"
scopes = ["root"]
instructions = "要求を分解し、Managerへ委譲して結果を統合する。"

[roles.director.delegation]
enabled = true
child_roles = ["manager", "worker"]
max_depth = 2
max_concurrency = 4

[roles.manager]
summary = "タスクの分解と統合"
scopes = ["session"]
instructions = "担当範囲をWorkerへ委譲し、結果を検証してcallerへ報告する。"

[roles.manager.delegation]
enabled = true
child_roles = ["worker"]
max_depth = 2
max_concurrency = 4

[roles.worker]
summary = "実行と検証"
scopes = ["session"]
instructions = "依頼された作業を実行し、結果と検証内容をcallerへ報告する。"

[roles.worker.delegation]
enabled = false
```

`defaults.root` / `defaults.session` は省略できる。各 role の `summary`、`scopes`、`instructions` は必須で、
`scopes` は `root` / `session` の一方以上を指定する。`delegation` block は任意だが、省略は既存catalogとの互換のため
従来の許可動作を維持する。新しい委譲policyでは leaf roleにも `enabled = false` を明記し、許可境界を曖昧にしない。
block 内で省略した場合は `enabled = false`、`child_roles = []`、`max_depth = 8`、`max_concurrency = 4` になる。

1ファイルは最大1 MiB、role IDは1–64 byteの小文字ASCII kebab-case、roleごとの`instructions`は最大16 KiBである。
未知field、空の`scopes`、未知scope、NUL、対応scopeを持たないdefaultは保存時または次のlaunch admissionで拒否する。
workspace layerで同じrole IDを定義するとglobalまたはbuilt-inの定義全体を置換するため、必要なfieldをすべて書く。

role は組織上の責務を prompt として与える。階層型チームで Director が小さいタスクを Worker へ直接 dispatch
する場合は 2 層、大きいタスクを Manager へ dispatch し、その Manager が Worker session を作る場合は
3 層になる。session の親は作成時の authenticated caller session として lifecycle state に固定し、既存 session
への dispatch では変更しない。dispatch binding は実行ごとの immediate caller を保持するため、完了報告は
Worker → Manager → Director と一段ずつ返る。`delegation` block を定義した role は daemon admission で
`enabled`、`child_roles`、`max_depth`、`max_concurrency` を検証し、prompt の自己申告には依存しない。block を
持たない role は従来の許可動作を維持する。depth admission も managed session では lifecycle state を直接読み、
作成完了直後の daemon crash で dispatch sidecar への複製が遅れても深さを過小評価しない。sidecar は lifecycle に
存在しない legacy / supervisor scope の fallback に限る。
durable supervisor run ではこれに加えて immutable な `ExecutionPolicy` が dispatch 総数・並列数・深さを制限する。

階層型チームでは、利用者がTUI/CLIから手動作成する新規sessionを調整役として扱うため、`defaults.session` は
`manager` とする。Director/Managerが実行者を委譲するときは `role = "worker"` を明示し、既定値に依存しない。
sidebar は各session名の横に `◆ Manager` / `● Worker` と階層インデントを常時表示し、Garden は `role-icon Role · parent › session` の nameplate と session 内の Agent を表すうさぎを表示する。Director drawerは `♛ Director` をrootとする親子ツリーを表示する。

`session_delegate_issue` のように worker launch を後で行う入口も、queued prompt に authenticated caller を保存する。
したがって launch 方法によらず同じ dispatch binding が作られ、worker の `session_complete` は直近の親 inbox だけへ届く。
子の inbox commit 後は、live な Manager には通知を送り、停止中なら通知を next-launch queue に永続化する。
いずれかの effective role に `delegation` block がある場合は、credential のない `session_delegate_issue` を拒否する。
`max_concurrency` は同じ親 session の実行中の子だけでなく未起動の delegated prompt も予約枠として数える。
root Agent からの委譲は workspace の root scope を1つの親として数え、Agent の runtime/model を変更しても
同じ親の利用数と絶対深度を引き継ぐ。上限判定と一時枠の取得は dispatch store の同じ lock 内で行い、session
作成や worker spawn の前に予約する。成功時は durable queue/run が枠を引き継ぎ、失敗時は guard が解放するため、並行 request
が check と publish の間をすり抜けない。session の親は終了 run の retention 対象ではない immutable lineage に保存し、古い
binding が削除された後も深度・sidebar・Garden の親子関係を維持する。

reader は future version、不正な role ID、空の `scopes`、未知 scope、16 KiB を超える instruction、NUL、対応 scope を許可しない
default を拒否する。workspace catalog の権威は target session branch ではなく daemon に登録された workspace root である。

## 割り当てと入口

CLI は optional `--role` を受け取る。

```text
usagi session create implementation --role worker
```

この例の `worker` は `hierarchical` / `flat` Team に含まれる。Team が `none` の場合は、先に
`roles workspace` または `roles global` で同じ ID の session-scope role を定義する。

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

`issue_enabled` / `memory_enabled` の effective 値は、Global 設定に **daemon に登録された workspace root** の
`.usagi/settings.json` を重ねて解決する。設定を tool 系統へ写す規則は `usagi-core` の `domain::agent::mcp_tools` に
1 つだけあり、`usagi mcp` が registry を組むときも同じ関数を通る。prompt が述べる系統と `tools/list` が登録する
系統が同じ設定から出ることは、この共有によって構造的に保証する。session worktree は `.usagi/settings.json` を
持たない（git 追跡外）ため、worktree を権威にすると workspace の上書きが消える。

設定が読めない場合は既定値へ倒さず launch を拒否し、error log に記録する。`usagi mcp` が serve loop の開始前に
失敗するのと同じ規則である。既定へ倒すと、自身の MCP server が登録できない tool 系統を prompt が述べた Agent が起動する。

## safe projection と非永続データ

`session_list` / `session_status` / `session_get` は `role_id` と current definition の `role_summary` を safe metadata として返す。list / status / overview の各 session は、これに加えて lifecycle state に作成時だけ保存した `parent_session_id` と、そこから導出する `parent_session_name` / `organization_depth` / `organization_path` を返す。path は root の `Director` から当該 session までで、既存 session への dispatch や同一 session 内の Agent handoff は階層を変更しない。
catalog が読めない場合も lifecycle metadata を返し、summary は `null` になる。

TUI は `role_id` / `role_summary`、lifecycle state 由来の `parent_session_id`、dispatch state 由来の `agent_status` を stable session identity keyed の controller projection として保持し、
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
