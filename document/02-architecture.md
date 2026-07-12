# 2. アーキテクチャ

> [ドキュメント目次](README.md) ｜ ← 前へ [1. プロジェクト概要](01-overview.md) ｜ 次へ → [6. 開発規約](06-conventions.md)

v2 の実装は **Cargo workspace 上の 4 クレート＋合成ルート（ルート bin パッケージ）** で構成する。
面（TUI / daemon / 入口）の境界をクレート境界に一致させ、依存方向を rustc で強制する。
本書がディレクトリ構成・クレート責務・依存ルールの正本である。

## 目次

- [なぜ 4 クレートか](#なぜ-4-クレートか)
- [ディレクトリ構成](#ディレクトリ構成)
- [各クレートの責務](#各クレートの責務)
- [依存ルール](#依存ルール)
- [クリーンアーキテクチャとの対応](#クリーンアーキテクチャとの対応)
- [単一バイナリと合成ルート](#単一バイナリと合成ルート)
- [CI・リリースとの整合](#ciリリースとの整合)
- [実装の置き場所ガイド](#実装の置き場所ガイド)
- [TUI Overview のコマンド dispatch](#tui-overview-のコマンド-dispatch)
- [TUI Closeup のコマンド dispatch](#tui-closeup-のコマンド-dispatch)
- [入口面 CLI のコマンド dispatch](#入口面-cli-のコマンド-dispatch)
- [入口面 MCP の tool dispatch](#入口面-mcp-の-tool-dispatch)
- [検討した代替案](#検討した代替案)

## なぜ 4 クレートか

v2 は「PTY 所有を daemon に移し、TUI は attach クライアントになる」設計
（[v1/document/proposals/02-daemon.md](../v1/document/proposals/02-daemon.md)）と、
「常駐しない入口（CLI / MCP）は daemon を権威とするクライアントにする」設計
（[proposals/01-entry-surfaces.md](proposals/01-entry-surfaces.md)）を前提にする。
この設計ではコードが自然に次の 4 つに分かれる。

- **daemon 面**: agent / シェルの PTY 所有・セッション監視・委譲 queue の消化（常駐サーバ側）。
- **TUI 面**: 画面描画・キー入力・attach プロトコルのクライアント側。
- **入口面（cli）**: 常駐しない入口。人間向け CLI サブコマンドとエージェント向け MCP サーバ。
- **共通（common）**: 各面が共有する domain エンティティ・usecase・IPC プロトコル型・永続化。

v1 は単一クレート内のモジュール分割だったため、層・面の依存方向はレビューでしか守れなかった。
v2 ではこの 4 分割をクレートとして表現し、「TUI が daemon の内部実装へうっかり依存する」類の
逆流をコンパイルエラーにする。

## ディレクトリ構成

```text
.
├── Cargo.toml            # workspace ルート ＋ 配布バイナリ usagi（bin）のパッケージ
├── src/
│   └── main.rs           # 合成ルート（実 IO の注入と実行面の dispatch のみ。COVERAGE_IGNORE 対象）
├── crates/
│   ├── core/             # usagi-core: 共通（common）
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── domain/          # エンティティ（他 usagi クレート非依存。外部は chrono/serde/uuid の基盤語彙のみ）
│   │       ├── usecase/         # 各面が共有するビジネスロジック
│   │       └── infrastructure/  # 各面が共有する外部接続（IPC プロトコル型・永続化・git）
│   ├── cli/              # usagi-cli: 入口面（常駐しない headless presentation）
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── cli/             # 人間向けサブコマンド（引数解析・dispatch・結果整形）
│   │       │   └── commands/         # サブコマンドハンドラ（store 系は core usecase 直呼び、session 系は daemon IPC）
│   │       └── mcp/             # MCP サーバ（stdio JSON-RPC の解釈・dispatch）
│   │           └── tools/            # tool アダプタ（commands と同じ core usecase を呼ぶ兄弟）
│   ├── daemon/           # usagi-daemon: daemon 面
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── presentation/    # IPC リクエストの dispatch・応答整形（daemon サーバ入口）
│   │       ├── usecase/         # daemon 専用ロジック（監視ティック・autostart queue consumer・通知調停・孤児 adopt 判定）
│   │       └── infrastructure/  # daemon 専用の外部接続（PTY 所有・IPC socket サーバ・daemon lifecycle 永続化）
│   └── tui/              # usagi-tui: TUI 面
│       └── src/
│           ├── lib.rs
│           ├── usecase/         # TUI に閉じた application ロジック（画面グラフの遷移・イベント状態機械）
│           │   ├── application        # 起動画面 EntryScreen と ScreenRunner への dispatch、Home controller
│           │   │   └── controller/    # 純粋 reducer、TUI-local backend port、FakeBackend
│           │   ├── terminal_input     # live pane の端末非依存入力語彙・bytes encoder・prefix classifier
│           │   ├── overview/          # Overview コマンドの解釈・dispatch
│           │   │   └── commands/          # 個別コマンドハンドラ（1 コマンド = 1 ファイル）
│           │   └── closeup/           # Closeup コマンドの解釈・dispatch
│           │       └── commands/          # 個別コマンドハンドラ（1 コマンド = 1 ファイル）
│           ├── infrastructure/  # attach クライアント（daemon への IPC クライアント側）・端末バックエンド
│           └── presentation/    # 画面描画・キー入力マッピング・起動バナー runner
│               ├── theme            # 色テーマ（意味的な役割→具体色の単一情報源。ANSI SGR を吐く）
│               ├── views/            # 各画面の view（splash / welcome / open / new / config / home）
│               │   ├── welcome            # トップメニュー（Open/New/Config/Quit ＋ recent 2 カラム。単体 workspace と unite を描き分け）の状態と描画
│               │   ├── open                # 登録済み workspace 一覧（名前＋最終利用の相対時刻＋選択中パス）の状態と描画
│               │   ├── new                 # 新規 workspace 作成フォーム（Clone/Existing 切替・入力フィールド・自動導出）の状態と描画
│               │   ├── config             # 設定画面（設定項目は未追加。ボディは空のプレースホルダ）の描画
│               │   ├── workspace          # ホーム画面（Switch／Closeup mode ＋ state-backed な左 session menu／右 tab pane）の状態と描画
│               │   ├── overview_modal      # コマンドパレット `:`（入力の前方一致で候補を絞る中央モーダル）の状態と描画
│               │   ├── closeup_modal       # セッションのアクションメニュー（フォーカス中セッションへの操作を選ぶ中央モーダル）の状態と描画
│               │   └── pr_modal            # Pull Request ポップアップ（PrLink 一覧＋選択中の詳細を出す中央モーダル）の状態と描画
│               ├── widgets/          # 画面をまたぐ再利用 UI 部品（下記）＋テキスト幅の測定・切詰め・折返し・相対時刻
│               │   ├── text_input        # 1 行キャレット編集バッファ（入力欄）
│               │   ├── icon              # usagi マスコット AA（アイコン）
│               │   ├── loading           # スピナー・進捗バー・ローディングうさぎ
│               │   └── modal             # 枠付きダイアログ（角丸 box・中央寄せ配置・既存 frame への overlay 合成）
│               └── layouts/          # 領域配置（ペイン分割・chrome＝枠/ヘッダ/フッタ/ステータス行）
│                   ├── mascot_screen      # マスコット＋タイトル＋中央寄せボディ＋固定フッタの共通全画面 chrome（welcome / config 等が共有）
│                   └── panes              # 左右 2 ペインの幅割り当てと結合（workspace 画面が使う）
└── v1/                   # 退避された旧実装（独立 Cargo プロジェクト。workspace exclude）
```

ディレクトリ名は `crates/<短い名前>`、パッケージ名は衝突回避のため `usagi-<名前>` とする
（`core` は Rust の組み込みクレート名と衝突するため、そのままパッケージ名にしない）。

## 各クレートの責務

| クレート | ディレクトリ | 責務 |
|---|---|---|
| `usagi-core` | `crates/core` | 各面が共有する domain / usecase / infrastructure（IPC プロトコル型・永続化・git） |
| `usagi-daemon` | `crates/daemon` | 常駐プロセス（`usagi daemon`）のサーバ側。PTY 所有・セッション監視・委譲 queue の消化を実装していく |
| `usagi-tui` | `crates/tui` | TUI クライアント側。画面描画・キー入力・attach プロトコルのクライアントを実装していく |
| `usagi-cli` | `crates/cli` | 入口面（常駐しない headless presentation）。人間向け CLI サブコマンド（`cli/`）とエージェント向け MCP サーバ（`mcp/`）を実装していく（設計は [proposals/01-entry-surfaces.md](proposals/01-entry-surfaces.md)） |
| `usagi`（bin） | ルート | 合成ルート。実 IO（標準入出力・引数・端末）を束ね、各面へ dispatch する |

### usagi-tui の内部構成

TUI 面はクレート内でクリーンアーキテクチャの層を切る（依存方向は
`presentation → usecase → domain ← infrastructure`。domain は共有のため
[usagi-core](#各クレートの責務) が持ち、tui はそれを参照する）。

| 層（`crates/tui/src/`） | 置くもの |
|---|---|
| `presentation/` | 画面描画・キー入力マッピング。描画は v1 と同じく自前の差分レンダリングで行い、UI フレームワークに依存しない。内部は各画面の view（`views/`）・再利用 UI 部品（`widgets/`）・領域配置（`layouts/`）に分け、view が layout で領域を割りそこへ widget を配置する。色は `theme`（意味的な役割 accent / success / danger … を具体色へ写す単一情報源。ANSI SGR を直接吐き外部クレートに依存しない）で一元管理する。対話ループもここに置く（`run` は `Terminal` ポートで毎フレーム描き直し、`Key` と注入された `WorkspaceLoader` を使って Welcome ⇄ Open / New / Config、Open ⇄ Workspace の画面グラフを回す。Workspace 内では Switch / Closeup の mode と Overview / PR の最前面 modal を状態機械で dispatch し、modal widget が組み立て済み workspace frame に枠を合成する。Recent は Welcome から Workspace へ直接進み、Esc で Welcome へ戻る） |
| `usecase/` | TUI に閉じた application ロジック。起動画面の `EntryScreen`、それを具体的な描画・入力実装へ委譲する `ScreenRunner` 境界、管理画面用の端末ポート `Terminal` と入力語彙 `Key`、live pane 専用の端末非依存入力語彙・bytes encoder・`Ctrl-O` classifier、Home の純粋 controller（state / event / effect reducer、TUI-local backend port と `FakeBackend`）、Overview / Closeup コマンドの解釈・dispatch、画面グラフの遷移、イベント処理の状態機械 |
| `infrastructure/` | daemon 端末へ attach する IPC クライアント側と端末バックエンド（raw mode・端末制御・キー/ホイール読み取り・クリップボード） |

`Terminal` は対話画面が使う端末の最小ポート（サイズ取得・フレーム描画・キー読み取り）で、`usecase` が
定義する。実端末の制御（crossterm による raw mode・代替スクリーン・キーイベントの `Key` への翻訳）は
合成ルート（ルートの `src/main.rs`）だけが実装し、`usagi-tui` は crossterm に依存しない。これにより
対話ループはフェイク端末で 100% ユニットテストでき、実端末 IO は計測対象外の合成ルートに閉じる。

**TUI の実装は core に吸収されない。** `usagi-core` が持つのは面をまたいで共有する
data（`domain/`）・IPC プロトコル型の定義・永続化・git（`infrastructure/`）と、
面をまたぐ共有ロジック（`usecase/`）だけである。描画・入力・画面遷移、および
attach の**クライアント側実装**は TUI に固有で、`usagi-tui` に置く。attach は
「プロトコル型は core・クライアント実装は tui」で分担する（daemon 側実装は
`usagi-daemon`）。したがって `usagi-tui` は core の薄いラッパではなく、
presentation 主体の実クレートになる。

## 依存ルール

```text
         usagi（bin, 合成ルート）
         │        │        │
         ▼        ▼        ▼
    usagi-tui  usagi-cli  usagi-daemon
         │        │        │
         └────┐   │   ┌────┘
              ▼   ▼   ▼
              usagi-core
```

- `usagi-tui` / `usagi-cli` / `usagi-daemon` は互いに依存**しない**。プロセス内の面選択は
  全面に依存できる合成ルートが要求型を変換する。daemon との実行時通信は IPC で行い、
  そのプロトコル型は `usagi-core` が持つ。
- `usagi-core` は他の usagi クレートに依存しない。
- `usagi-core` の `domain/` は他層（`usecase` / `infrastructure`）にも依存しない。外部クレートは
  エンティティの基盤語彙に限る — 時刻を表す `chrono`、JSON インデックス表現を導出する
  `serde`、v2 resource incarnation を表す `uuid` だけを使い、git・PTY・端末・ファイル IO 等の重い外部クレートは持ち込まない
  （それらは `infrastructure/` の責務）。
- 外部クレートの version はルート `Cargo.toml` の `[workspace.dependencies]` で一元管理し、
  必要になった時点で追加する（v1 の依存を先回りで持ち込まない）。
- lint 設定は `[workspace.lints]` に置き、各クレートは `[lints] workspace = true` で継承する。

## クリーンアーキテクチャとの対応

4 層（`presentation → usecase → domain ← infrastructure`）はクレート分割後も維持する。
層とクレートの対応は次のとおり。

| 層 | 置き場所 |
|---|---|
| domain | `usagi-core` の `domain/` |
| usecase | 面をまたぐ共有は `usagi-core` の `usecase/`。片面専用のロジックは各面クレート内 |
| infrastructure | 面をまたぐ共有（IPC プロトコル型・永続化・git）は `usagi-core` の `infrastructure/`。片面専用（PTY は daemon、端末描画は tui）は各面クレート内 |
| presentation | 各面クレート（TUI の画面 / daemon のサーバ端点 / cli のサブコマンド・MCP tool アダプタ）と、ルート `main.rs` の dispatch |

依存方向は「クレート間」（tui / cli / daemon → core）と「core 内モジュール」（usecase → domain ← infrastructure）
の両方のレベルで守る。実 IO は合成ルートで注入し、各クレートは依存注入によりユニットテスト可能に保つ。

## 単一バイナリと合成ルート

配布物は従来どおり**単一バイナリ `usagi`** のまま。第 1 引数で面を選ぶ。

| 起動 | 面 |
|---|---|
| `usagi`（引数なし） | TUI 面（`usagi-tui`） |
| `usagi daemon` | daemon 面（`usagi-daemon`） |
| `usagi mcp` | 入口面の MCP（`usagi-cli` の `mcp/`） |
| その他のサブコマンド | 入口面の CLI（`usagi-cli` の `cli/`）。実行結果の TUI 要求は合成ルートが TUI 面へ委譲 |

個々のコマンドと起動面の対応は [1. プロジェクト概要#現在の実装状態](01-overview.md#現在の実装状態)が
正本である。

ルートを bin パッケージとして維持する理由:

- リリース起点はまだ v1（auto-release は `v1/Cargo.toml` の version を監視し、release.yml は
  v1 をビルドする。[6. 開発規約#リリース](06-conventions.md#リリース) が正本）。version をルートに
  リテラルで置き続ければ、v2 初リリース時に監視・ビルド対象をルートへ切り替えるだけでリリース機構が動く。
- インストール・利用手順（単一バイナリ配布）が変わらない。

内部クレート（`crates/*`）は `publish = false` とし、`version` を持たない
（配布 version はルートパッケージだけが持つ。version の二重管理によるドリフトを避ける）。

## CI・リリースとの整合

| 対象 | workspace 化との整合 |
|---|---|
| coverage | `cargo llvm-cov --workspace` で crates/ 配下も計測される。`COVERAGE_IGNORE` は合成ルート `src/main.rs` のみ |
| test / clippy | ルートで実行するとルートパッケージしか対象にならないため、`--workspace` を付ける（test.yml / lefthook / recommend-tests の fail-safe も同様） |
| auto-release | リリース起点はまだ v1（`v1/Cargo.toml` の version を監視）。ルートにはリテラル `version` を維持しておき、v2 初リリース時に監視対象をルートへ切り替える |
| release-build-check / release.yml | まだ v1 を対象に release ビルドする（`--manifest-path v1/Cargo.toml`）。v2 初リリース時に root bin のビルドへ切り替える |
| `v1/` | `[workspace] exclude` で計測・ビルド対象外。`v1/**` を変更する push / PR は v1-test.yml が v1 のマニフェストで検証する |

## 実装の置き場所ガイド

v1 から機能を再実装するときの置き場所の指針。

| 実装 | 置き場所 |
|---|---|
| `Workspace` / `Settings` / `Issue` などのエンティティ、および画面が並べて見せる読み取り値（`WorkspaceOverview` = workspace＋各カウント、`UniteOverview` = 合併した workspace 群の合計、welcome 画面の recent 一覧が持つ `Recent` = そのどちらか） | `crates/core/src/domain/` |
| `state.json` などの store・IPC プロトコル型・git 操作 | `crates/core/src/infrastructure/` |
| workspace の登録・touch・recent overview 構築、セッション作成・設定解決など両面が使うロジック | `crates/core/src/usecase/` |
| PTY 所有・IPC socket サーバ・daemon 永続化（daemon 専用の外部接続） | `crates/daemon/` の `infrastructure/` |
| セッション監視ティック・autostart queue consumer・通知調停（daemon 専用ロジック） | `crates/daemon/` の `usecase/` |
| IPC リクエストの dispatch・応答整形（daemon サーバ入口） | `crates/daemon/` の `presentation/` |
| 各画面の描画（view） | `crates/tui/` の `presentation/views/` |
| 画面をまたぐ再利用 UI 部品（widget） | `crates/tui/` の `presentation/widgets/` |
| 色（意味的な役割→具体色）・色定数 | `crates/tui/` の `presentation/theme` |
| 領域配置・ペイン分割・chrome（layout）。マスコットを頂く全画面の共通 chrome は `layouts/mascot_screen`（welcome / config が共有） | `crates/tui/` の `presentation/layouts/` |
| 画面グラフの遷移・イベント状態機械 | `crates/tui/` の `usecase/`。Home controller は `usecase/application/controller.rs` で state / event / effect を純粋に還元し、daemon wire を TUI-local backend port の外へ閉じる |
| Overview コマンドの解釈・dispatch | `crates/tui/` の `usecase/overview/`（ハンドラは `overview/commands/`） |
| Closeup コマンドの解釈・dispatch | `crates/tui/` の `usecase/closeup/`（ハンドラは `closeup/commands/`） |
| CLI から選ばれた TUI 起動画面の dispatch | `crates/tui/` の `usecase/application.rs`（CLI 要求からの変換はルート `src/main.rs`） |
| attach クライアント・端末バックエンド | `crates/tui/` の `infrastructure/` |
| CLI サブコマンドの引数解析・dispatch・結果整形 | `crates/cli/` の `cli/`（ハンドラは `cli/commands/`） |
| MCP サーバ（JSON-RPC の解釈・dispatch・tool アダプタ） | `crates/cli/` の `mcp/`（アダプタは `mcp/tools/`） |
| 各面への dispatch と実 IO の注入 | ルート `src/`（実 IO の注入のみ。テスト可能なロジックは crates へ） |

## TUI Overview のコマンド dispatch

`crates/tui` の `usecase/overview/` は、Overview 固有のコマンド語彙
（`config` / `env` / `issue` / `session`）を持つ。
`interpret` は入力をトップレベル名と trim 済みの未解釈引数に分け、`Command::into_handler` が
コマンドごとのハンドラへ変換する。ハンドラは `Run` トレイトを実装し、実 IO や画面状態を
直接操作せず純粋な `CommandResult` を返す。コマンド固有処理を持たない現在のハンドラは、
解釈したコマンド名と引数を `CommandResult::NotImplemented` に保持して返す。

```text
入力 ─► interpret ─► Command ─► Command::into_handler() ─► Box<dyn Run> ─► CommandResult
          │                                                               │
          └─ 空入力 / 未知名 ─► ParseError
```

- **コマンド一覧**: private な registry が名前・説明・usage と解釈 factory の単一情報源で、
  `commands()` が表示用 metadata を名前順に返す。
- **引数の所有**: Overview の入口はトップレベル名だけを解釈する。サブコマンド・オプションの文法は
  個別ハンドラが所有できるよう、残りを未解釈文字列のまま渡す。
- **dispatch**: `Command::into_handler` がコマンドとハンドラの対応を 1 か所に集約する。ハンドラは
  **1 コマンド = 1 ファイル**（`usecase/overview/commands/<command>.rs`）に置く。
- **副作用の分離**: IF は入力の解釈とハンドラ dispatch までを担い、実 IO・画面遷移・共有 usecase・
  daemon IPC を実行しない。

## TUI Closeup のコマンド dispatch

`crates/tui` の `usecase/closeup/` は、Closeup 固有のコマンド語彙
（`agent` / `chat` / `close` / `diff` / `terminal`）を持つ。`interpret` は入力を
トップレベル名と trim 済みの未解釈引数に分け、`Command::into_handler` がコマンドごとの
ハンドラへ変換する。ハンドラは `Run` トレイトを実装し、実 IO や画面状態を直接操作せず
純粋な `CommandResult` を返す。コマンド固有処理を持たない現在のハンドラは、解釈した
コマンド名と引数を `CommandResult::NotImplemented` に保持して返す。

```text
入力 ─► interpret ─► Command ─► Command::into_handler() ─► Box<dyn Run> ─► CommandResult
          │                                                               │
          └─ 空入力 / 未知名 ─► ParseError
```

- **コマンド一覧**: private な registry が名前・説明・usage と解釈 factory の単一情報源で、
  `commands()` が表示用 metadata を名前順に返す。
- **引数の所有**: Closeup の入口はトップレベル名だけを解釈する。サブコマンド・オプションの文法は
  個別ハンドラが所有できるよう、残りを未解釈文字列のまま渡す。
- **dispatch**: `Command::into_handler` がコマンドとハンドラの対応を 1 か所に集約する。ハンドラは
  **1 コマンド = 1 ファイル**（`usecase/closeup/commands/<command>.rs`）に置く。
- **副作用の分離**: IF は入力の解釈とハンドラ dispatch までを担い、実 IO・画面遷移・共有 usecase・
  daemon IPC を実行しない。

## 入口面 CLI のコマンド dispatch

`crates/cli` の `cli/` は、コマンド面の骨格（枠）を持つ。ここに置くのは
**ターミナルから `usagi <cmd>` で叩く人間向けコマンド**（`open` / `config` / `doctor` /
`update` / `completion` / `version` と clap 自動の `help`）だけで、エージェント向けの
issue / memory 操作は MCP 面（`crates/cli/mcp/`）が受け持つ。どんなコマンド・オプションが
あるかは `clap` のコマンドツリー（`Cli` / `Command`）で定義し、`usagi --help` と型の
両方から見える。dispatch は `Run` トレイトで一様化する。

```text
argv ─► clap 解析 ─► Command ─► Command::into_handler() ─► Box<dyn Run> ─► Run::run(out)
                       │                                                        │
                       │                                           ┌─► Exit(code)
                       │                                           └─► LaunchTui(request) ─► 合成ルート ─► TUI
                       └─ 解析エラー / --help / --version ─► 整形して out|err へ
```

- **`Run` トレイト**: 各コマンドの実行を `fn run(&self, out) -> io::Result<RunOutcome>` に一様化する。
  巨大な match ではなく、コマンドごとのハンドラ型が `Run` を実装する。ハンドラは
  **1 コマンド = 1 ファイル**（`cli/commands/<command>.rs`）に置く。
- **dispatch**: `Command::into_handler` が解析済みコマンドを対応ハンドラに変換する 1 か所の対応付け。
- **エントリ `run(args, version, out, err)`**: 実 IO を注入して受け取り、終了コードまたは
  TUI 起動要求を `RunOutcome` で返す。`TuiRequest` は CLI 側の意図であり、合成ルートが
  TUI 側の `EntryScreen` へ変換するため、面クレートどうしは依存しない。
  `args` は単相化を増やさないよう `Vec<OsString>` の具体型で受ける。配布 version は
  ルートパッケージだけが持つ（[単一バイナリと合成ルート](#単一バイナリと合成ルート)）ため、
  `--version` の値は合成ルートから注入し、clap コマンドに載せる。
- TUI 起動要求を返すコマンドの対応は
  [1. プロジェクト概要#現在の実装状態](01-overview.md#現在の実装状態)が正本である。
- **`completion`** は実装済み: `clap_complete` で `Cli` のコマンドツリーから対象シェル
  （`clap_complete::Shell`）の補完スクリプトを生成して標準出力へ出す。定義が唯一の真実なので
  補完候補は CLI の実態と一致する。ただし静的ジェネレータの仕様上、`hide = true` の内部コマンド
  （`hop` / `agent-phase` / `guard-workspace`）も補完候補には含まれる（`--help` には出ない）。
- **`update`** は実装済み: usagi 自体の新版確認。リリースは GitHub の `v<semver>` タグなので、
  `usagi-core` の git seam（`infrastructure::git::GitRunner`）で `git ls-remote --tags <repo>` を
  実行し、その出力から最大 semver を取り出して配布 version と比較して 1 行報告する。出力パースと
  比較は `commands::update` の純粋関数（ユニットテスト）、git の実 IO（`GitRunner` の本実装）は
  合成ルートに閉じる（新規クレート依存は無く git を再利用）。
- **内部フックコマンド**: usagi はエージェント起動時に、Claude の `PreToolUse` フックへ
  `usagi guard-workspace`（worktree 外へのツール呼び出しを拒否）を、Stop フックへ
  `usagi agent-phase <phase>`（ライフサイクル phase 報告）を配線する。この 2 つは人間向けでは
  ないため `--help` に出さない（`hide = true`）。呼び手（人手でもエージェントの推論でもなく
  エージェントのハーネスが自動実行）も目的も人間向けコマンドと違うので、ハンドラは
  `cli/commands/` ではなく **`cli/hooks/`** に分離する（clap の `Command` ツリーと `Run`
  dispatch は共有）。MCP tool と違い Claude のフックはシェルコマンドしか呼べないため、この統合は
  CLI コマンドとして持つしかない。フックは終了コードだけを見るので、いまは黙って正常終了する
  スタブ（guard-workspace は許可のみで**まだ enforcing ではない**）。

## 入口面 MCP の tool dispatch

`crates/cli` の `mcp/` は、エージェント向けの tool 面（IF）を持つ。CLI が人間向けの
`usagi <cmd>` を提供するのに対し、MCP は issue / memory / session の tool を JSON-RPC で
公開する（設計は [proposals/01-entry-surfaces.md](proposals/01-entry-surfaces.md)）。CLI の
`Run` トレイトに対応する一様化を `Tool` トレイトで行う。

```text
stdin ─► serve ─► handle_line ─► respond(method) ┬─ initialize ─► serverInfo/capabilities
 (1 行 = 1 JSON-RPC)                              ├─ tools/list ─► registry の name/description/inputSchema
                                                  └─ tools/call ─► dispatch(name, args) ─► Tool::call
```

- **`Tool` トレイト**: `name` / `description` / `input_schema`（`tools/list` に載る IF）と
  `call`（実行）を持つ。`call` は既定が未実装スタブで、中身を実装する tool だけが
  オーバーライドする（枠＝既定実装）。tool は **系統ごとにファイル**（`mcp/tools/issue.rs` /
  `memory.rs` / `session.rs`）に置き、各 tool が 1 struct として実装する。
- **レジストリと dispatch**: `tools::registry()` が全 tool を連結し、`mcp::dispatch(name, params)` が
  名前で引いて `call` を呼ぶ。CLI の `Command::into_handler` に対応する一様な経路。
- **serve ループ**（`mcp/serve.rs`）: stdio 上の JSON-RPC 2.0 を 1 行ずつ処理する。純粋な
  ルーティング（`handle_line`: str → 応答 or 通知の無応答）と実 IO の反復（`serve`）を分け、
  応答エンベロープの整形は `mcp/protocol.rs` に集約する。`initialize` と `tools/list` は実際に
  応答し、`tools/call` は tool を名前で引いて呼ぶ（各 `call` は未実装スタブなので今は
  「未実装」エラーを返す）。配布 version は合成ルートが `serve` に注入する。
- CLI のコマンドハンドラと MCP の tool は **同じ core usecase を呼ぶ兄弟**で、共有ロジックは
  すべて `usagi-core` に置く（[入口面 CLI のコマンド dispatch](#入口面-cli-のコマンド-dispatch)）。

## 検討した代替案

構成を決めたときの設計判断の記録。

| 代替案 | 不採用の理由 |
|---|---|
| 単一クレート内のモジュール分割（v1 方式） | 面・層の依存方向をコンパイラで強制できない。ビルド・テストのクレート単位並列性も得られない |
| 層ごとのクレート分割（domain / usecase / infrastructure / presentation を各クレート化） | 実行面（TUI / daemon）の境界を表現できず、daemon 専用と TUI 専用の infrastructure が同じクレートに同居する |
| TUI / daemon を別バイナリとして配布 | リリース CI（4 プラットフォーム）と配布手順の変更が大きい。単一バイナリ＋サブコマンドなら現行リリース機構が無変更で使える |
