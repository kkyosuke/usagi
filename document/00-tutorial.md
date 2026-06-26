# 0. チュートリアル（はじめての usagi）

> [ドキュメント目次](README.md) ｜ 次へ → [1. プロジェクト概要](01-overview.md)

`usagi` を**インストールから「Agent でセッションを並行で走らせる」まで**を一通りなぞる導入ガイドです。
各機能の詳細仕様には正本へのリンクを張ります（本書は手順に絞り、仕様は重複させません）。

## 目次

- [全体の流れ](#全体の流れ)
- [1. インストール](#1-インストール)
- [2. プロジェクトを初期化する（usagi init）](#2-プロジェクトを初期化するusagi-init)
- [3. TUI を起動する（usagi hop）](#3-tui-を起動するusagi-hop)
- [4. セッションを作って Agent を起動する](#4-セッションを作って-agent-を起動する)
- [5. もう 1 つセッションを足して並行で走らせる](#5-もう-1-つセッションを足して並行で走らせる)
- [6. 行き来と入力待ちの通知](#6-行き来と入力待ちの通知)
- [次に読むもの](#次に読むもの)

## 全体の流れ

```text
install ─▶ cd <project> ─▶ usagi init ─▶ usagi hop ─▶ [Open でワークスペースを選ぶ]
                                                          │
                                                          ▼
                                              切替(Switch) ─ : ─▶ パレットで session create
                                                          │  ┌─ c ─▶ 別セッションをその場で作成
                                                          ▼  │
                                              在席(Focus) で agent ─▶ 没入(Attached) で claude を操作
```

下の手順はこのまま順に実行できます。モード（**切替・在席・没入**）とキーの考え方は
[design/05-home.md](design/05-home.md#モードと状態遷移切替在席没入) が正本です（`Ctrl-O`＝切替へズームアウト、
`Esc`＝一段戻る、`:`＝[コマンドパレット（統括）](design/05-home.md#コマンドパレット統括overview)、終了は `Ctrl+C`）。

## 1. インストール

macOS / Linux はワンライナーでインストールできます（ビルド済みバイナリを `~/.usagi/bin` に配置）。

```bash
curl -fsSL https://raw.githubusercontent.com/KKyosuke/usagi/main/scripts/install.sh | bash
export PATH="$PATH:$HOME/.usagi/bin"   # 案内に従って PATH を通す
```

プラットフォーム別アーカイブ・ソースからのビルド（`cargo install --path .`）は
[README#Installation](../README.md#installation) を参照してください。導入後、依存ツールの状態を確認できます。

```bash
usagi doctor   # git / bash / Agent CLI（claude・codex・sakana.ai・gemini）/ 通知 / 設定ストレージの健全性を ok / warn / missing で表示
```

`usagi doctor` の詳細は [03-commands/01-cli.md#usagi-doctor](03-commands/01-cli.md#usagi-doctor) を参照。

## 2. プロジェクトを初期化する（usagi init）

開発したいリポジトリのディレクトリへ移動し、`usagi init` でワークスペースとして登録します。

```bash
cd ~/git/my-app     # 開発したいディレクトリ（git リポジトリでなくてもよい）
usagi init          # .usagi/ を初期化し、グローバルレジストリ workspaces.json に登録
```

- まだ clone していないなら `usagi init --git <URL>` で「clone + 登録」を一度に行えます。
- `.usagi/` の中身（`state.json` / `settings.json` / `issues/` / `sessions/`）と `.gitignore` の扱いは
  [01-overview.md#プロジェクト構造](01-overview.md#プロジェクト構造) と
  [data/02-workspace.md](data/02-workspace.md) を参照。

## 3. TUI を起動する（usagi hop）

```bash
usagi hop
```

起動画面（Welcome）が開きます。`Open`（`o`）を選び、先ほど初期化したワークスペースを選択すると
**ホーム画面**に入り、既定の**切替（Switch）**モードになります。

- 左ペイン … セッション一覧（先頭は常設の **ルート行 `⌂ root`**）。キーボードが乗り `↑↓` で選びます。
- コマンドパレット … `:`（コロン）で開き、`session` / `issue` / `config` などワークスペース全体のコマンドを実行する入力面。
- 画面・各モードの見た目は [design/05-home.md](design/05-home.md)、画面遷移は
  [design/README.md](design/README.md#画面遷移図) を参照。

## 4. セッションを作って Agent を起動する

`:` で**コマンドパレット**を開き、セッション（ワークスペース配下の各 git リポジトリに同名ブランチの
worktree を張る作業単位）を作ります。`create` は `c` / `new` に短縮できます。

```text
:                                 （コマンドパレットを開く）
session create feature-login      （または短縮形：session c feature-login）
```

作成すると `.usagi/sessions/feature-login/` 配下に worktree が構築され、そのセッションが
アクティブになって**在席（Focus）**へ移ります。右ペインにアクション UI（既定は Menu）が出るので、
`agent` を選びます（メニューでは `a`、Prompt なら `agent` と入力）。

```text
Run a command:
  > terminal  Open a shell
    agent     Launch the agent
```

`agent` を実行すると右ペインに埋め込みシェルが開き、設定中の Agent CLI（既定 `claude`）が起動して
**没入（Attached）**に入ります。起動時に usagi の issue MCP サーバ（`usagi mcp`）が組み込まれるので、
エージェントは起動直後から `issue_*` tool でタスクを操作できます。

- 没入中の**予約キーは `Ctrl-O`・`Ctrl-T`・`Ctrl-N`/`Ctrl-P`（`Ctrl-←`/`Ctrl-→`）・`Ctrl-G`**で、ほかのキー（`Esc` や `Ctrl-W` を含む）はすべて
  シェルへ流れます。`Ctrl-O` の単押しで**切替**へズームアウト、`Ctrl-T` で**在席へズームアウトしてアクションメニュー**（terminal / agent などを選ぶ画面）を開き、`Ctrl-N`/`Ctrl-P`（または `Ctrl-←`/`Ctrl-→`）で**没入のままタブ（ペイン）を前後へ
  切り替え**、`Ctrl-G` で**没入のまま agent タブを追加**します
  （その分シェル / Agent 側の同キーは奪われます）。`Ctrl-W` は奪わずシェルの「直前の単語を削除」として流すため、タブを閉じるのは切替（`Ctrl-O` で抜けて `x`）で行います。terminal タブの追加は在席のアクションメニュー（`Ctrl-T` で開く）か切替（`t`）でも行えます。
- 素のシェルだけ欲しいときは `agent` の代わりに `terminal` を使います。Agent を動かしたまま `Ctrl-T` で在席のアクションメニューを開き、
  そこから terminal を選べば同じセッションに terminal タブが増え（切替へ抜けて `t` でも可）、`Ctrl-N`/`Ctrl-P`（`Ctrl-←`/`Ctrl-→`、または切替の `←`/`→`）でタブを行き来できます。
- `agent` / `terminal` の仕様は [03-commands/02-tui.md](03-commands/02-tui.md#agent)、MCP の組み込みは
  [03-commands/03-mcp.md](03-commands/03-mcp.md) を参照。

## 5. もう 1 つセッションを足して並行で走らせる

別のタスクを並行で進めたいときは、いまの Agent を**止めずに**新しいセッションを作れます。

1. 没入中に **`Ctrl-O`**（単押し）を押して**切替（Switch）**へズームアウトします（キーボードが左ペインへ移ります）。
2. 左ペインで **`c`** を押すと、その場でインラインの名前入力が開きます。新しいセッション名（例 `feature-search`）を
   入力して `Enter` で作成 → そのセッションの**在席**へ移ります。
3. 在席で再び **`agent`** を起動すれば、2 つ目のセッションでもエージェントが走ります。

```text
没入 ──Ctrl-O──▶ 切替(Switch) ──c──▶ 名前入力 ──Enter──▶ 在席(Focus) ──agent──▶ 没入(Attached)
```

各セッションのシェルは画面を開いている間「ターミナルプール」に常駐するので、行き来しても終了しません。
切替で別のセッションを選んで `Enter`（または `l`）すれば、ライブなセッションへはそのまま再アタッチ、
アイドルなら在席へ移ります。セッションの破棄は `:` で開くコマンドパレットの `session remove <name>`（名前を省くと選択モーダル）。
ライフサイクルの概念は [04-orchestration.md](04-orchestration.md) を参照。

## 6. 行き来と入力待ちの通知

複数セッションを並行で走らせると、左ペインの各エントリ 2 行目で状態がひと目で分かります。

| 表示 | 意味 |
|---|---|
| `▶ running`（緑） | Agent が稼働中 |
| `◆ waiting`（黄） | Agent が入力待ち（あなたの応答を待っている） |
| `⏸ idle`（シアン） | Agent が終了してアイドル |

アタッチしていない（裏で走っている）セッションが入力待ち・完了に変わると、デスクトップ通知も出ます
（設定 `notifications_enabled` が ON のとき）。状態検知の仕組みは
[04-orchestration.md#Agent フックによる状態報告](04-orchestration.md#agent-フックによる状態報告)、画面表示と通知の挙動は
[design/05-home.md#使用中-agent-の表示入力待ちの検知と通知](design/05-home.md#使用中-agent-の表示入力待ちの検知と通知) を参照。

## 次に読むもの

- [1. プロジェクト概要](01-overview.md) — usagi が解決する課題と全体構造。
- [3. コマンドリファレンス](03-commands/README.md) — CLI / TUI 内コマンド / MCP サーバの一覧と詳細。
- [4. オーケストレーション](04-orchestration.md) — セッション・worktree のライフサイクル。
- [5. 設定](05-settings.md) — Agent CLI・通知・ローカル LLM などの設定。
- [design/05-home.md](design/05-home.md) — ホーム画面の 3 モードとキー操作。
