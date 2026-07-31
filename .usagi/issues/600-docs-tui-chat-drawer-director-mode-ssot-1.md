---
number: 600
title: docs(tui): chat drawer を「指示モード（Director mode）」として命名し、名称の SSoT を 1 つにする
status: todo
priority: medium
labels: [tui, docs, naming, agent, refactor]
dependson: []
related: [576, 580, 597]
created_at: 2026-07-31T11:07:06.532956+00:00
updated_at: 2026-07-31T11:07:06.532956+00:00
---

## 目的

root scope の Agent と対話するあの面（現 "Workspace Agent drawer" / UI 表示 `󰚩 chat`）に**モード名**を与え、
名称の正本を 1 つにする。ユーザーはこの面を「指示（ディレクター）モード」と呼んでおり、実装・UI・ドキュメントの
呼び名がそれぞれ違うことが混乱の原因になっている。

## 背景

同じものに 3 系統の名前が付いている。

| 層 | 現在の呼び名 |
|---|---|
| UI（header button / drawer title） | `󰚩 chat` |
| ドキュメント（`document/03-tui.md` の節・`02-architecture.md`） | Workspace Agent drawer |
| コード（module / 型 / key / state） | `workspace_agent_drawer`, `WorkspaceAgentDrawerProjection`, `WorkspaceAgentNew`, `AppKey::ToggleWorkspaceAgentDrawer`, `workspace_agent_drawer_open` |

#580 で「ユーザー向け表示から内部機能名 `Workspace Agent` を外す」対応はしたが、`chat` は面の**役割**を表していない。
この面は root scope（`session_id: None`）の Agent に対して指示を出し、session を作らせる面であり、
managed session の Closeup（実作業を見る面）とは役割が異なる。名前でその役割差を表す。

## 決定する名称

- **ユーザー向け（日本語）**: 指示モード
- **英語 / identifier**: director（`Director mode`）
- **UI 表示**: header button と drawer title を `󰚩 director` にする（glyph は現行の `CHAT_ICON` を維持）
- **面の中の 1 本**: 現行どおり conversation（変更しない）
- **chord**: 現行どおり `Ctrl-O Ctrl-G` を維持する。`Ctrl-O d` は ScrollDown に割り当て済みで衝突するため、
  名前に合わせて chord を変えない。

## 変更方針

- ユーザー向け名称の正本を `document/03-tui.md` の節見出しに置き、他のドキュメントはそこへリンクする
  （SSoT 規約）。節見出しは「指示モード（Director mode）」とし、目次・パンくず・アンカーリンクを揃える
  （アンカー不一致は CI の markdown link check で落ちる）。
- UI 文字列（header button・drawer title・footer key hints）を新名称に合わせる。footer の hint は
  `Ctrl-O n / New: choose CLI · Esc / Ctrl-O Ctrl-G: close` の情報量を落とさない。
- コード identifier を `workspace_agent_*` → `director_*` に揃える。少なくとも次を同じ変更で一貫させる。
  - `presentation/views/workspace_agent_drawer.rs` の module 名と公開型
  - `AppKey::ToggleWorkspaceAgentDrawer` / `AppKey::OpenWorkspaceAgentNew`、`AppState::workspace_agent_drawer_open`、
    `WorkspaceAgentNew`
  - `LiveTerminalAction::WorkspaceAgent` / `WorkspaceAgentNew`
  - `document/02-architecture.md` の module 一覧の記述
- 表示名の変更と identifier の rename を**同じ PR**に載せる（片方だけ変えると名前が 4 系統に増える）。

## 受け入れ条件

- ユーザーが目にする文字列（header button・drawer title・footer hints・doc 本文）に `Workspace Agent` と `chat` が
  面の名前として残っていない。
- `document/03-tui.md` に「指示モード（Director mode）」の節があり、他ドキュメントからの参照はリンクで済んでいる
  （同じ説明を 2 か所に書かない）。
- コード identifier が `director` 系に統一され、`workspace_agent` 系の残骸が無い。
- chord・key routing・入力優先順位（#580 / #581 の契約）は不変である。
- 既存テストの assertion 文字列が新名称に更新されている。

## テスト方針

- `cargo test -p usagi-tui`（表示文字列 assertion の更新）
- `cargo test -p usagi --bin usagi` / `cargo test -p usagi --test cli_tui_pty`（shipping 表示の更新反映。PTY E2E は
  単独実行する）
- `lychee --config lychee.toml --no-progress '*.md' 'document/**/*.md' '.agents/**/*.md' '.github/**/*.md'`
  （節見出し変更に伴うアンカー検証）

## 非目標

- chord / key binding の変更。
- drawer の geometry・入力優先順位・conversation 投影規則の変更。
- v1 側（`v1/`）の名称変更。
