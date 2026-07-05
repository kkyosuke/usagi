# 設計計画: gemini/agy への MCP 注入・セッション単位の agent 指定・利用可能 agent フィルタ

> 本ファイルは実装前の**設計計画ドラフト**（`document/` の SSoT ではない）。確定した設計は
> 各 issue の実装 PR で `document/` に反映する。

## 0. ゴール

ユーザー要望の 3 点をまとめて解決する。

1. **gemini / agy（Antigravity）でも MCP を設定できるようにする**（現状は Claude / Codex(+codex-fugu) のみ）。
2. **session 委譲時に agent を指定できるようにする**（`session_create` / `session_delegate_issue`）。
3. **利用可能な agent だけを選択させる**（(a) インストール済み ∧ (b) MCP 注入可能 の 2 観点でフィルタ／検証）。

---

## 1. 現状の裏取り（コードを開いて確認した事実）

ブリーフの記載を実コードで検証し、以下を確定した（差分は「★」）。

### 1.1 Agent port と MCP 注入

- Agent port は `src/domain/agent.rs` の `Agent` トレイト。メソッドは
  `program` / `launch_command(&AgentWiring, resume, initial_prompt) -> String` /
  `has_resumable_session` / `forget_session` / `headless_command`。
  **`launch_command` は純粋な文字列生成で `Result` を返さず、副作用を持たない**。
- `AgentWiring`（同ファイル）は `usagi_bin` / `local_llm_model` / **`model`** を持つ。
  `model` は **既に全アダプタが描画する注入点**だが、`Settings::agent_wiring` が常に `None` を入れている
  （= PR #577 が通した「口」。供給源未実装。#099 が担当）。★ ブリーフの「model 注入点」は本フィールド。
- CLI → adapter 写像は `src/infrastructure/agent/mod.rs::agent_for(cli)`（唯一の選択点）。
- MCP 注入は現状 Claude（`claude.rs::mcp_config_json`、`--mcp-config` JSON を serde で生成）と
  Codex（`codex.rs::mcp_server_overrides`、`-c key=value` TOML 上書き）のみ。注入サーバは
  `usagi`（常時, `<bin> mcp`）と `usagi-llm`（`local_llm.enabled` 時, `<bin> llm-mcp --model <m>`）。
- gemini（`gemini.rs`）/ agy（`antigravity.rs`）は設計コメントどおり **MCP / フック / system-prompt を
  inline 注入しない**。worktree 注意書きは開始プロンプト（`-i=` / `-p`）の先頭に前置するのみ。

### 1.2 ★ MCP 対応可否の SSoT が既にある（重要）

- `src/domain/agent_feature.rs` が **agent × 機能のサポート行列の SSoT**。`AgentFeature::Mcp` /
  `LocalLlmMcp` / `PhaseReporting` / `SystemPrompt` / `InitialPrompt` / `Resume` / `ForgetHistory` を持ち、
  `support(cli, feature) -> Support{Yes,No}` が **exhaustive match**（新 CLI 追加は here を書くまでコンパイルエラー）。
- 現状 `support(Gemini|Antigravity, Mcp) == No` / `LocalLlmMcp == No` / `SystemPrompt == No` / `PhaseReporting == No`。
- `usagi feature` コマンド（`presentation/cli/feature.rs`）がこの行列を表として描画する。
- **したがって「MCP 対応可否」の判定は本行列 1 か所**で、要件 3 の (b) はこれを参照すればよい。
  gemini/agy に MCP を足す（要件 1）＝ここの `Mcp`/`LocalLlmMcp` を `Yes` に反転すること、と定義できる。

### 1.3 利用可否（インストール済み）判定

- `src/usecase/agent.rs::available_clis(runner)` が `AgentCli::ALL` を `runner.available(cmd)` で絞る。
- `available` の実体は `doctor/runner.rs::SystemRunner::available` → **`<cmd> --version` を叩く**。
  ★ `agy --version` は無い/遅い可能性があり、`available_clis` の誤検知・レイテンシ懸念は実在（要件 3 で対策）。
- TUI の agent ピッカー（`home/ui/panes.rs`、`state.installed_agents()`）と config 画面
  （`config/state`）は **既に `available_clis` の結果で選択肢を絞っている**。「インストール済みだけ選ばせる」
  土台は TUI 側に既にある。

### 1.4 session への agent 指定・永続化

- `session_create`（`mcp/session.rs::CreateArgs` は `name` のみ）/ `session_delegate_issue`
  （`mcp/usagi.rs::DelegateIssueArgs` は `number` + 任意 `name`）は **agent を受け取らない**。
  `usecase/session::create` シグネチャも `(workspace_root, name)`。
- ★ `usagi session` CLI サブコマンドは **存在しない**（session 操作は MCP のみ。`main.rs` の
  `Commands` に `Session` は無い）。
- ★ セッション単位の agent 永続化は無い。ただし **ペイン単位 `StoredPane.cli: Option<AgentCli>`
  （`open_panes_store.rs`）は既に round-trip している** — 復元時 `restore_open_panes` が
  `agent_for(pane.cli.unwrap_or(default_cli))` で同じ agent を再起動する。
- ★ 起動経路は TUI home に 4 つ（`home/mod.rs`）: `open_terminal`（前景起動）/ `start_pending_spawn`
  （背景 spawn）/ `restore_open_panes`（復元）/ `autostart_queued_prompts`（委譲プロンプト自動起動）。
  各経路が独立に `cli` を解決する。前景は「一回限りの `agent_choice`（`agent <name>` コマンド由来）
  ?? `default_agent()`（= `settings.agent_cli` 由来）」。**委譲直後の自動起動と初回起動は
  `default_agent()`（グローバル）を使う** → ここが「session_create(agent=X) を実際に効かせる」ための鍵。
- `session::remove` は **既に `agent: &dyn Agent` を受け取る**（usecase が domain port に依存する前例）。
  本番の `AgentBackend::remove`（`main.rs::CliAgentBackend`）が `agent_for(settings.agent_cli)` を解決して渡す。
  一方 `prompt`/`send`（queue/live）は **agent 非依存**（テキストをストアにキューするだけ）。

### 1.5 既存 issue との関係（★ 最重要の発見）

| issue | 状態 | 内容 | 本計画との関係 |
|---|---|---|---|
| **#099** | todo/high | `session_create`/`session_delegate_issue` に `agent_cli`/`model` を追加し、`SessionRecord` に記録、起動時に実効設定より優先解決、#577 の model 注入点へ接続 | **要件 2 の正本。新規作成せず、これを使う** |
| **#120** | todo/medium | `model_flag_parts` 三重定義・**MCP サーバ台帳の二重定義**・agent-phase 語彙を SSoT 化 | **gemini/agy MCP 注入（要件 1）の前提**。ブリーフが「#060」と呼んだのは実際はこれ |
| **#119** | todo/medium | gemini / antigravity アダプタをパラメータ化して統合 | gemini/agy MCP 注入と同じ 2 ファイルを触る。related |
| #058 | done | agent 起動コマンド生成を domain から外へ | 背景（port の副作用配置を考える際の前例） |
| #060 | done | CLI/MCP の **JSON 整形** SSoT 化（≠ MCP config） | ブリーフの誤認訂正：MCP config の話ではない |

→ **要件 2 は #099 が既にカバー**。本計画で新規起票するのは **要件 1（gemini/agy MCP 注入）と
要件 3（利用可能∧MCP対応フィルタ）だけ**。#099 は新規 issue に `dependson`/`related` で接続する。

---

## 2. 設計方針

### 2.1 論点1: gemini / agy の MCP 注入方式

**結論: worktree ローカルに設定ファイルを書き出し、git-exclude する（skills 配線の踏襲）。**

- **書き込み先**: `~/.gemini/...` のようなユーザー設定ではなく、**そのセッションの worktree 内**
  （例: gemini は `<worktree>/.gemini/settings.json`、agy は同等のプロジェクトスコープ設定）。理由:
  - usagi は既に `<worktree>/.claude/skills/*` を **worktree 内に symlink し git-exclude**している
    （`session::create` の skills 配線）。同じ「worktree 内に書く＋除外」の前例があり、方針変更を最小化できる。
  - worktree はセッション破棄（`session remove` / `usagi clean`）で丸ごと消えるため、**後始末が自動**。
    ユーザー設定を汚さず、並行セッション間でも干渉しない。
  - ブリーフが挙げた「ユーザ設定やリポジトリに書き込まない」方針の変更範囲を **worktree 内（＝ session の
    使い捨てツリー）に限定**でき、リポジトリ本体・ユーザー設定は不変を保てる。
- **git を汚さない**: 書き出したファイルは `git::ensure_all_excluded`（skills と同じ仕組み）で
  worktree の exclude に登録し、セッションが dirty 判定されないようにする。
- **フォーマット**: gemini は `settings.json`（`mcpServers` キー）、agy は `mcp_config.json`。
  **具体パスとキー構造は実装時に `gemini --help` / `agy --help` と実挙動で確定**（issue A の調査ステップ）。
  現行 `antigravity.rs` は agy が `~/.gemini/antigravity-cli/` を使うことを掴んでいる（history.jsonl）ので、
  プロジェクトスコープ設定の可否をここで検証する。プロジェクトスコープが無理なら、worktree パスをキーにした
  ユーザー設定への追記＋`forget_session` 相当での掃除、を次善策とする（この分岐は :root へ要相談）。
- **注入内容の SSoT**: 注入する MCP サーバ台帳（`usagi` / `usagi-llm`）は現状 claude.rs と codex.rs で
  二重定義。gemini/agy 用の 3・4 個目のエンコーダを足すと重複が悪化するため、**#120 の「`AgentWiring`
  由来の中立記述 `Vec<(name, cmd, args)>`」を先に用意し、それを gemini/agy の書き出しでも描画する**。
  → issue A は **#120 に dependson**（SSoT を作ってから 4 エンコーダで共有）。
- **matrix 更新**: `agent_feature::support` の `Gemini`/`Antigravity` × `Mcp`/`LocalLlmMcp` を `Yes` に反転。
  `PhaseReporting`（フック機構が無い）と `SystemPrompt`（引き続き開始プロンプト前置で代替）は **No のまま**
  （本要件は MCP のみ）。行列テストも更新。
- **ドキュメント**: `document/02-architecture.md`（agent アダプタ節）・`document/03-commands/03-mcp.md`
  （どの CLI に MCP が wire されるか）・必要なら `document/05-settings.md` を更新。

### 2.2 論点2: 副作用（設定ファイル書き出し）を port にどう載せるか

**結論: `Agent` トレイトに provision 系メソッドを 1 本追加する（adapter 内隠れ副作用は採らない）。**

```
// domain/agent.rs（案）
/// worktree に、この CLI が MCP 等を読み込むための設定を（必要なら）書き出す。
/// inline 注入で足りる CLI（Claude/Codex）は no-op。gemini/agy は
/// worktree ローカル設定ファイルを書き、git-exclude する。冪等。
fn provision(&self, wiring: &AgentWiring, dir: &Path) -> std::io::Result<()> { Ok(()) }
```

- **なぜ port に置くか**: `launch_command` は純粋な文字列で `Result` を返さず、テストからも呼ばれる。
  ここに fs 副作用を混ぜると契約が壊れる。副作用は別メソッドに分離し、`launch_command` の純粋性を保つ。
- **依存方向**: メソッド宣言は domain（`Path` と `io::Result` のみ、外部クレート非依存）、実装は
  infrastructure（fs アクセス）。`session::remove` が既に `&dyn Agent` を受ける前例どおり、**domain port を
  usecase/presentation が呼ぶ**構図で依存方向（domain ← infra、presentation → usecase → domain）を壊さない。
- **呼び出し箇所**: TUI の 4 起動経路（`open_terminal` / `start_pending_spawn` / `restore_open_panes` /
  `autostart_queued_prompts`）で **`launch_command` の直前に `agent.provision(&wiring, dir)`** を呼ぶ。
  Claude/Codex は no-op なので既存挙動不変。
- **後始末**: 書き出し先が worktree 内なら `session remove` のツリー削除で消える（追加の deprovision 不要）。
  provision は冪等（毎起動で上書き）とし、復元起動でも安全。worktree 外に書く次善策を採る場合のみ、
  既存の `forget_session`（remove 時に呼ばれる）に掃除を相乗りさせる。
- **代替案（不採用）**: `launch_command` 内で書き出す案は、戻り値が `String` でエラーを表現できず、
  テストで実 fs を触るため却下。

### 2.3 論点3（= 要件2）: session への agent 指定 API — **#099 が正本**

#099 のスコープ（`session_create`/`session_delegate_issue` に `agent_cli`/`model` を追加 →
`SessionRecord` に記録 → 4 起動経路で実効設定より優先解決 → #577 の model 注入点へ接続）を採用。
本計画からの補足設計:

- **永続化先**: `SessionRecord`（`domain/workspace_state.rs`、`state.json`）に
  `agent_cli: Option<AgentCli>`（`#[serde(default, skip_serializing_if=Option::is_none)]`、旧ファイル互換）
  を追加。★ **ペイン単位 `StoredPane.cli` では不足**：委譲直後の自動起動・初回起動時にはまだペインが無く、
  現状 `default_agent()`（グローバル）が使われるため。session レコードに持たせて初回・自動起動の既定にする。
- **launch 解決**: 4 起動経路の `cli` 解決を「**その session の `SessionRecord.agent_cli` ?? 実効
  `settings.agent_cli`**」に変更（`agent_choice` の一回限り上書きは従来どおり最優先）。以後の再起動は
  `StoredPane.cli` が引き継ぐ（既存挙動）。
- **API 形**: `usecase/session::create` に agent 引数を追加（`Option<AgentCli>`）。MCP 側 `CreateArgs` /
  `DelegateIssueArgs` に任意 `agent_cli`（と #099 が扱う `model`）を追加。`session_delegate_issue` は
  `usagi.rs::tool_delegate_issue` が `create_session` へ引き渡す。
- **CLI/TUI**: `usagi session` CLI は存在せず新設不要（session は MCP 専用が設計）。TUI は既存の
  `agent <name>` 一回限り選択で足り、必須ではない（#099 も「あれば一貫」程度）。
- **論点5（後方互換）**: `agent_cli` 未指定なら従来どおり実効 `settings.agent_cli` にフォールバック。
  `state.json` は旧フィールドのみでも読める（serde default）。

### 2.4 論点4（= 要件3）: 利用可能 agent フィルタ / 検証

**結論: `available_clis`（インストール）∩ `agent_feature::support(_, Mcp)==Yes`（MCP対応）を新 usecase に。**

- 新 usecase（`usecase/agent.rs`）:
  ```
  pub fn mcp_capable_clis(runner: &dyn CommandRunner) -> Vec<AgentCli>
  // = available_clis(runner) を agent_feature::support(cli, Mcp)==Yes で更に絞る
  ```
  `available_clis` は温存（インストール済みだけを見たい TUI ピッカー用途は残る）。
- **gemini/agy との関係**: MCP 対応可否は `agent_feature` 行列が握るので、**issue A（matrix 反転）が
  landした瞬間に gemini/agy が自動で候補に入る**。要件 3 のロジック自体は A に依存しない（行列を読むだけ）。
  「A の前は候補外／A の後に解禁」が行列 1 か所の更新で自然に成立する。
- **委譲時の検証方針**: `session_create`/`session_delegate_issue` の `agent_cli` 指定は、
  - **インストール済みでない** → ツールエラー（明確なメッセージ、`mcp_capable_clis` を候補列挙）。
  - **MCP 非対応** → 委譲作業は usagi MCP を要するため **原則エラー**（委譲の意味が薄い）。ただし
    「非対応でも起動は許す（警告のみ）」も選択肢。**この厳格さは :root へ要相談**（§4）。
  - `agent_cli` 省略時は従来フォールバック（検証しない）。
- **tool description**: `session_create`/`session_delegate_issue` の inputSchema の `agent_cli` 説明に
  「利用可能な候補」を動的列挙するのは MCP がヘッドレス（起動時にプロセス probe が走る）で高コストなので、
  **description は静的文言＋エラー時に候補列挙**とする。
- **agy `--version` レイテンシ対策**: `available`（`<cmd> --version`）が agy で無い/遅い問題は、
  - まず `mcp_capable_clis` は行列で先に絞れる（agy が Mcp=Yes になってから probe されるので影響は A 後のみ）。
  - probe に **タイムアウト**を入れる、または agy は `--version` 以外の軽量判定（PATH 存在 =`which`）に
    切り替える案を issue B で扱う（`CommandRunner::available` の実装改善）。既存の TUI ピッカーは probe を
    別スレッドで走らせて結果を後追い適用しているので、UI ブロックは既に回避済み。

### 2.5 論点6: テスト方針（カバレッジ 100% 維持）

- **provision の DI**: provision は `dir: &Path` を受けるので、テストは tempdir を渡して
  「gemini/agy 用ファイルが書かれた／内容が正しい／Claude・Codex は no-op」を検証。実 IO は worktree パス
  経由で注入され、既存の runner DI / worktree-keyed store のテストパターンに揃う。
- **matrix**: `agent_feature` の行列テスト（現行あり）を gemini/agy=Mcp/LocalLlmMcp=Yes に更新。
- **mcp_capable_clis**: 既存 `available_clis` の `FakeRunner` を流用しユニットテスト。
- **launch 解決**: `SessionRecord.agent_cli ?? settings.agent_cli` の解決は usecase 側に純粋関数として
  切り出し、presentation の薄い配線だけを残す（IO は合成ルートで束ねる、既存方針）。
- **MCP 引数**: `CreateArgs`/`DelegateIssueArgs` の新引数パースと検証分岐を `mcp/session.rs` /
  `mcp/usagi.rs` のユニットテスト（`FakeBackend`）で網羅。

---

## 3. issue 分割・依存関係・優先度

### 3.1 新規起票（本計画で作成済み）

| ID | タイトル要旨 | priority | dependson | related | 要件 |
|---|---|---|---|---|---|
| **#134** | feat(agent): Gemini/Antigravity(agy) に MCP を注入（worktree ローカル設定書き出し + Agent port `provision`） | high | **#120** | #119, #99 | 要件1 |
| **#135** | feat(agent): インストール済み∧MCP対応の agent 列挙 usecase と委譲時の検証（agy `--version` 対策含む） | high | — | #99 | 要件3 |

### 3.2 既存 issue の位置づけ・推奨更新

- **#099（要件2の正本）**: 新規作成せず流用。**推奨: `dependson = [135]`（検証ヘルパを使うため）、
  `related = [134, 120]`**。※この #099 更新は本セッションで未適用（`status` は単一書き手規約で触らない。
  `dependson`/`related` の追記可否は :root へ確認 → §4-3）。
- **#120**: #134 の前提。可能なら #134 着手前に「MCP サーバ台帳の中立化」部分を先行完了させる。
- **#119**: #134 と同じ 2 ファイル（gemini/antigravity アダプタ）を触るため、片方 land 後にもう片方を追従（related）。

### 3.3 依存グラフ

```
#120 (MCP台帳SSoT) ──▶ #134 (gemini/agy MCP注入) ──▶ (matrix: Mcp=Yes)
                                     │                      │
                                     └──related──▶ #119     ▼
                                                   #135 (available∧MCP対応フィルタ) ─▶ #099 (session agent指定)
                                                                                       （推奨 dependson #135 / related #134）
```

読み: **#120 → #134** で gemini/agy が MCP 対応になり、**#135** が「利用可能∧MCP対応」を列挙・検証、
**#099** がその検証を使って session 単位 agent 指定を実装する。#135 と #099 は claude/codex だけでも
先行可能（#134 が land すれば gemini/agy が自動で候補入り）。

### 3.4 推奨着手順

1. **#120**（MCP 台帳 SSoT）→ 2. **#134**（gemini/agy MCP 注入＋provision port＋matrix 反転）
3. **#135**（`mcp_capable_clis` + 委譲検証 + agy probe 対策）→ 4. **#099**（session 単位 agent 指定）

---

## 4. :root へ上げる決定事項（設計上の分岐）

1. **委譲時の MCP 非対応 agent の扱い**: `session_create`/`session_delegate_issue` に MCP 非対応の
   `agent_cli` が来たとき **エラー**（推奨・委譲は usagi MCP 前提）か、**警告して起動を許す**か。
2. **agy のプロジェクトスコープ MCP 設定が不可だった場合の書き込み先**: worktree ローカルが無理なら
   ユーザー設定（`~/.gemini/...`）への worktree キー付き追記＋`forget_session` 掃除、という次善策を採るか。
   （方針「ユーザー設定を汚さない」の一部緩和になるため要判断）
3. **#099 の依存更新を本セッションで行ってよいか**（`dependson=[B]` / `related=[A,120]` の追記）。
   `status` は触らない前提。
