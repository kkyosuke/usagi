# 14. session role

> [設計提案一覧](README.md) ｜ 関連: [Agent dispatch MCP](08-agent-dispatch-mcp.md) / [Agent launch boundary](../02-architecture.md#agent-launch-boundary)

## 1. 目的

workspace root の Agent を `director`、managed session を `manager` / `coder` / `reviewer` などとして動かし、
複数 session を責務の異なる worker として分業できるようにする。role の種類と指示本文は workspace ごとに追加・変更できる。

role は Agent runtime（Claude / Codex）や model とは独立した**作業方針**である。role を OS sandbox、MCP authorization、
session lifecycle の権限として扱わない。たとえば `reviewer` を指定するとレビュー中心の指示になるが、それだけで filesystem を
read-only にするとは約束しない。権限制御が必要なら role とは別の capability / sandbox policy として設計する。

## 2. モデル

```text
global catalog + workspace catalog         daemon-owned session incarnation
<data-dir>/roles.toml                      sessions.json
<workspace>/.usagi/roles.toml
┌─────────────────────┐                  ┌────────────────────────────┐
│ effective defaults  │  create/dispatch │ session_id / worktree_id   │
│ effective roles     │ ───────────────► │ role_id = "reviewer"       │
└──────────┬──────────┘                  └──────────────┬─────────────┘
           │ current definition                         │ stable assignment
           └──────────────────────┬─────────────────────┘
                                  ▼ launch / resume
                    scope safety prompt
                    + role instructions
                    + optional local-LLM instructions
                                  ▼
                           product adapter
```

概念は次の 3 つに分ける。

| 概念 | 例 | 変更特性 | 権威 |
|---|---|---|---|
| scope | root / managed session | launch identity から決まり変更不可 | daemon の `LaunchScope` |
| role assignment | `director` / `reviewer` | session incarnation の作成時に固定 | daemon lifecycle store |
| role definition | role の説明と instruction | global で再利用し workspace で上書き可能。次回起動から反映 | global / workspace role catalog |

session 名に `review-*` のような命名規則を持たせて role を推測しない。名前は人間向けの identity、`role_id` は typed metadata
として別に保存する。また Agent ごとに role を保存せず session に保存する。同じ worktree 内の Agent が互いに異なる責務を名乗る
曖昧さを避けるためである。異なる role が必要なら session を分ける。

## 3. global / workspace 設定

role catalog は 2 層で持つ。global は個人が複数 workspace で再利用する role、workspace は repository 固有の role と既定値を持つ。
runtime/model allowlist の `.usagi/config.toml` とは分離し、model 選択と作業指示のレビュー差分を混ぜない。

| scope | 保存先 | 用途 | 編集面 |
|---|---|---|---|
| global | `<data-dir>/roles.toml` | 個人共通の role と fallback defaults | TUI `Config > Roles > Global` |
| workspace | `<workspace>/.usagi/roles.toml` | repository で共有する role、global role の置換、workspace defaults | TUI `Config > Roles > Workspace` またはファイル編集 |

`<data-dir>` は既存 settings と同じ `$USAGI_HOME` → `~/.usagi` および runtime mode の解決結果を使う。workspace catalog は
git 管理できるため、プロジェクト固有の分業方針を PR でレビューできる。

実効 catalog は global に workspace を重ねる。workspace に同じ ID があれば、field 単位に混ぜず role 定義全体を置き換える。
defaults は `workspace → global → compatibility default` の順で最初に存在する値を使う。削除 marker や多段 inheritance は初期契約へ
入れない。この単純な規則なら、画面と daemon が同じ結果を再現できる。

```toml
version = 1

[defaults]
root = "director"
session = "coder"

[roles.director]
summary = "全体方針を決め、作業を session に委譲する"
scopes = ["root"]
instructions = """
成果を分解し、manager / coder / reviewer session へ委譲する。
自分で実装せず、依存関係・進捗・統合判断を管理する。
"""

[roles.manager]
summary = "担当領域を分解し、worker を調整する"
scopes = ["session"]
instructions = """
担当領域の計画と受け入れ条件を明確にし、必要な作業を別 session へ委譲する。
結果を検証して director へ要約する。
"""

[roles.coder]
summary = "実装と検証を行う"
scopes = ["session"]
instructions = """
依頼された変更を実装し、リスクに応じたテストを実行して成果を報告する。
"""

[roles.reviewer]
summary = "差分を独立にレビューする"
scopes = ["session"]
instructions = """
要求、正しさ、回帰、安全性、テスト不足の順に確認する。
指摘は根拠と重要度を添え、問題がなければその旨を明示する。
"""
```

設定 reader は次を検証する。

- `version` は既知の値だけを受理する。future version を既知 schema として解釈しない。
- role ID は小文字 ASCII の kebab-case、1–64 byte とする。
- `summary` は表示専用、`instructions` だけを prompt に含める。instruction は role ごとに 16 KiB を上限とし、NUL を拒否する。
- `scopes` は `root` / `session` の非空集合であり、defaults が指す role は対応 scope を許可する。
- prompt は child process の argv に現れ得るため、credential、token、環境変数値などの secret を設定へ置かない。

両方の設定ファイルが無い既存 workspace では、現在の root prompt と session worktree prompt のみを使う互換モードにする。
壊れた設定や未知 role を黙って `coder` に読み替えない。明示 role を伴う create / dispatch は spawn 前に
`invalid_argument` とし、既存 session の観測と削除は継続できる。

## 4. 割り当て API

role を**定義する場所**は Config / catalog、どの session に使うかを**決める場所**は session create である。
TUI の Create Session modal に role picker、CLI に `--role`、MCP の session selector に optional `role` を加える。

```text
usagi session create review-auth --role reviewer
```

MCP では次のように指定する。

```json
{"name":"session_dispatch","arguments":{
  "session":{"name":"review-auth","role":"reviewer"},
  "agent":{"runtime":"codex","model":"gpt-5"},
  "prompt":"認証変更をレビューする"
}}
```

同様に `session_create` / `session_delegate_issue` / `session_delegate_brief` は top-level の `role` または
`session.role` を受ける。入口ごとに場所を変えず、session selector を持つ新 API は `session.role`、session 名を top-level に持つ
legacy API は移行期間だけ top-level `role` とする。

| 対象 | `role` 省略時 | `role` 指定時 |
|---|---|---|
| 新規 root Agent | `defaults.root` | root scope を許可する role だけ受理 |
| 新規 managed session | `defaults.session` | session scope を許可する role だけ受理 |
| 既存 managed session | 保存済み `role_id` を使う | 保存値と一致すれば冪等。不一致は conflict |
| legacy session（role 無し） | 従来の generic session prompt | 明示 adoption 操作まで role を暗黙付与しない |

`session_get` / `session_list` / `session_status` は safe metadata として `role_id` と catalog の現在の `summary` を返す。
instructions 本文は通常の一覧応答へ含めない。TUI は role ID を badge として表示できるが、表示値を lifecycle identity や
authorization に使わない。

role には「定義」と「割り当て」の 2 種類の変更があり、扱いを分ける。

| 変更 | 途中で可能か | 反映タイミング |
|---|---|---|
| `reviewer.instructions` の編集 | 可能 | live Agent は変えず、同じ role の次回 launch / explicit resume から反映 |
| session の `role_id` を `coder` から `reviewer` へ変更 | 初期契約では不可 | 新しい reviewer session を作る |

割り当てを不変にするのは、実行中 Agent、queued run、provider resume の会話が別の責務へ途中変化するのを防ぐためである。
役割を変えたい場合は session を新規作成し、必要な context を prompt や commit で引き継ぐ。将来 `session_role_set` を足す場合も
live / queued / resumable Agent が一つもない状態に限定し、独立した durable operation とする。

## 5. daemon での解決と prompt 合成

client / MCP server は role instruction を wire へ載せず、role ID だけを送る。role 省略時も client ではなく daemon が effective default を
決定する。daemon は session reservation の直前に global / workspace catalog を読み、role と scope の組を検証して、解決済みの
`ManagedSession.role_id` を保存する。これにより MCP の起動時 schema snapshot を authorization として信用せず、runtime/model の再検証と
同じ fail-closed 境界を保てる。

workspace catalog は target session の branch ではなく、daemon が所有する registered workspace root から読む。session が自分の
worktree 内の `.usagi/roles.toml` を編集しても、その変更は PR で基点へ統合されるまで実効 policy にならない。assignment と launch で
異なる checkout を読んで結果が揺れることを防ぐ。

Agent の launch / explicit resume では daemon provisioner が保存済み `role_id` を session scope resolver から得て、current catalog の
instruction を読み直す。保存するのは role ID だけで、render 済み prompt、設定 path、argv は durable launch snapshot に入れない。
設定変更は live process を変えず、次の launch / resume から反映される。

prompt の合成順は固定する。

```text
1. code-defined scope prompt       root/session の責務と filesystem 境界
2. effective role prompt          director/coder/reviewer/manager の作業方針
3. code-defined optional suffix   trusted local-LLM delegation
```

role instruction は `<role id="reviewer">...</role>` のように境界を付ける。scope prompt は code-defined のまま先頭に置き、role 定義で
置換できない。Claude では合成済み文字列を単一 `--append-system-prompt` 値、Codex 系では単一
`developer_instructions=<TOML string>` 値として既存 provision に渡す。initial user prompt へ連結しない。

## 6. director と manager の関係

`director` は root の既定 role とし、workspace 全体の要求分解、session dispatch、結果の統合を担当する。`manager` は managed session の
role であり、割り当てられた領域内をさらに分解して別 session へ dispatch できる。既存 MCP credential / dispatch binding が caller と
worker の provenance を保持するため、role ごとの独自 routing は追加しない。

```text
director (root)
├─ manager (session: backend)
│  ├─ coder (session: backend-impl)
│  └─ reviewer (session: backend-review)
└─ coder (session: docs)
```

role は組織図を表現するが、信頼境界を拡張しない。manager も session worktree の外を直接編集できず、他 session への仕事は
authenticated `session_dispatch` で依頼する。worker の完了は既存 dispatch binding により直接 caller inbox へ戻る。

## 7. 実装境界

| 層 | 変更 |
|---|---|
| core domain | canonical `RoleId`、`RoleScope`、`ManagedSession.role_id`、safe projection |
| core infrastructure | global / workspace `roles.toml` reader、deterministic merge、size/schema validation |
| core usecase / IPC | create/dispatch の optional role selector、既存 session の一致検証 |
| daemon | reservation 前の current catalog 検証、session scope から provision への role 解決 |
| product adapters | 既存の単一 system-prompt 引数へ合成済み prompt を渡すだけ |
| MCP | schema と orchestration guide に role selector を公開 |
| TUI | Global / Workspace role editor、create form の role picker、session row の role badge（catalog 不正時も lifecycle 操作は維持） |

導入は domain/config reader、daemon lifecycle、prompt composition、MCP、TUI の順に分ける。最初の互換 migration では既存
`ManagedSession` の `role_id` を `Option` + serde default で読み、generic prompt の意味を維持する。全既存 session へ current default を
自動書き込みしない。

## 8. 受け入れ条件

- 同じ runtime/model で作った `coder` と `reviewer` session に異なる role instruction が一度だけ注入される。
- root の既定は `director`、managed session の既定は `defaults.session` になり、scope 不一致 role は spawn 前に拒否される。
- 同名 session への同一 role dispatch は冪等、不一致 role dispatch は既存 assignment を変更せず conflict になる。
- role catalog の変更は live Agent と durable snapshot を書き換えず、次回 launch / resume にだけ反映される。
- role instruction を wire response、dispatch store、runtime snapshot、log に保存しない。
- legacy session、設定無し workspace、malformed / future-version catalog の互換・fail-closed 挙動を固定する。
- role が reviewer でも filesystem 権限が暗黙に変わらず、既存 scope guard / sandbox test がそのまま通る。

## 9. 採用しない案

| 案 | 採用しない理由 |
|---|---|
| session 名の prefix から role を推測 | rename・命名揺れ・衝突に弱く、stable identity と表示上の分類が混ざる |
| role ごとに Claude/Codex profile を増やす | product adapter と作業方針が直積になり、model allowlist と prompt の責務が混ざる |
| prompt 本文を dispatch request で受ける | caller が安全境界を置換でき、監査可能な workspace policy が失われる |
| Agent/run ごとに role を持つ | 1 worktree 内の責務が不明確になり、session を分業単位にする目的と合わない |
| role instruction を lifecycle store に複製 | 設定変更時に二重の正本が生まれ、secret を durable state へ残す危険が増える |
| role から filesystem/MCP 権限を暗黙導出 | prompt policy と強制可能な security policy は保証レベルが異なる |
