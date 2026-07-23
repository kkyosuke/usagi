# 2. アーキテクチャ

> [ドキュメント目次](README.md) ｜ ← 前へ [1. プロジェクト概要](01-overview.md) ｜ 次へ → [3. TUI](03-tui.md)

v2 の実装は **Cargo workspace 上の 4 クレート＋合成ルート（ルート bin パッケージ）** で構成する。
面（TUI / daemon / 入口）の境界をクレート境界に一致させ、依存方向を rustc で強制する。
本書がディレクトリ構成・クレート責務・依存ルールの正本である。

## 目次

- [なぜ 4 クレートか](#なぜ-4-クレートか)
- [ディレクトリ構成](#ディレクトリ構成)
- [各クレートの責務](#各クレートの責務)
- [Markdown 永続化の commit contract](#markdown-永続化の-commit-contract)
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
（[v1/document/proposals/02-daemon.md](../v1/document/04-orchestration.md)）と、
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
│   ├── main.rs           # 面の選択だけを担う合成ルート
│   ├── runtime/          # 実 IO adapter（各面のライブラリ port を接続）
│   │   ├── cli.rs        # CLI outcome、実 git、TUI / daemon への bridge
│   │   ├── daemon.rs     # Unix socket・signal・process・daemon record / lock
│   │   └── tui.rs        # crossterm terminal と workspace filesystem adapter
│   └── tui_input.rs      # crossterm event を TUI 非依存の入力語彙へ変換
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
│   │       ├── presentation/    # daemon サーバ入口（daemon verb と IPC request の dispatch・応答整形）
│   │       │   └── ipc.rs       # handshake 後の IPC protocol handler
│   │       ├── usecase/         # daemon 専用ロジック（lifecycle verb、terminal/runtime・orchestration）
│   │       └── infrastructure/  # daemon 専用の外部接続（Unix socket transport）
│   │           └── unix_transport.rs # generation locator と peer credential を検証する Unix transport
│   └── tui/              # usagi-tui: TUI 面
│       └── src/
│           ├── lib.rs
│           ├── usecase/         # TUI に閉じた application ロジック（画面グラフの遷移・イベント状態機械）
│           │   ├── application        # 起動画面 EntryScreen と ScreenRunner への dispatch、Home controller
│           │   │   ├── controller/    # Entry/Home の純粋 reducer、typed attach effect、TUI-local fake backend
│           │   │   ├── pane/          # Closeup tab / placeholder の純粋 reducer
│           │   │   └── pane_runtime/  # daemon inventory / stream を pane へ結合する client state
│           │   ├── terminal_input     # live pane の端末非依存入力語彙・bytes encoder・prefix classifier
│           │   ├── overview/          # Overview コマンドの解釈・dispatch
│           │   │   └── commands/          # 個別コマンドハンドラ（1 コマンド = 1 ファイル）
│           │   └── closeup/           # Closeup コマンドの解釈・dispatch
│           │       └── commands/          # 個別コマンドハンドラ（1 コマンド = 1 ファイル）
│           ├── infrastructure/  # attach クライアント（daemon への IPC クライアント側）・端末バックエンド
│           └── presentation/    # 画面描画・キー入力マッピング・起動バナー runner
│               ├── frame            # ANSI/Unicode 幅をセル grid にする pure frame diff（端末 write は adapter 側）
│               ├── theme            # 色テーマ（意味的な役割→具体色の単一情報源。ANSI SGR を吐く）
│               ├── views/            # 各画面の view（splash / welcome / open / new / config / home）
│               │   ├── welcome            # トップメニュー（Open/New/Config/Quit ＋ recent 2 カラム。単体 workspace と unite を描き分け）の状態と描画
│               │   ├── open                # 登録済み workspace 一覧（名前＋最終利用の相対時刻＋選択中パス）の状態と描画
│               │   ├── new                 # 新規 workspace 作成フォーム（Clone/Existing 切替・入力フィールド・自動導出）の状態と描画
│               │   ├── config             # 設定画面（global/workspace scope の draft・明示 save・失敗時 retry）の状態と描画
│               │   ├── workspace          # ホーム画面（Switch／Closeup mode ＋ state-backed な左 session menu／右 tab pane）の状態と描画
│               │   ├── overview_modal      # Overview コマンドパレット `:`（入力の前方一致で候補を絞る中央モーダル）の状態と描画
│               │   ├── closeup_modal       # Closeup コマンドメニュー（フォーカス中セッションへの操作を選ぶ中央モーダル）の状態と描画
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
| `usagi-daemon` | `crates/daemon` | 常駐プロセス（`usagi daemon`）のサーバ側。daemon lifecycle verb、IPC server protocol、daemon-owned terminal / runtime の usecase と Unix transport を持つ |
| `usagi-tui` | `crates/tui` | TUI クライアント側。画面描画・キー入力・attach プロトコルのクライアントを実装していく |
| `usagi-cli` | `crates/cli` | 入口面（常駐しない headless presentation）。人間向け CLI サブコマンド（`cli/`）とエージェント向け MCP サーバ（`mcp/`）を実装していく（設計は [proposals/01-entry-surfaces.md](proposals/01-entry-surfaces.md)） |
| `usagi`（bin） | ルート | 合成ルート。実 IO（標準入出力・引数・端末）を束ね、各面へ dispatch する |

### usagi-tui の内部構成

TUI 面はクレート内でクリーンアーキテクチャの層を切る（依存方向は
`presentation → usecase → domain ← infrastructure`。domain は共有のため
[usagi-core](#各クレートの責務) が持ち、tui はそれを参照する）。

| 層（`crates/tui/src/`） | 置くもの |
|---|---|
| `presentation/` | 画面描画・キー入力マッピング。描画は v1 と同じく自前の差分レンダリングで行い、UI フレームワークに依存しない。`frame` は ANSI/Unicode 幅を考慮して view の行を cell grid にし、row / column span の pure diff を返す。surface reset と geometry 変更は full clear と全行 repaint にし、実端末への cursor 移動・write は adapter に閉じる。内部は各画面の view（`views/`）・再利用 UI 部品（`widgets/`）・領域配置（`layouts/`）に分け、view が layout で領域を割りそこへ widget を配置する。色は `theme`（意味的な役割 accent / success / danger … を具体色へ写す単一情報源。ANSI SGR を直接吐き外部クレートに依存しない）で一元管理する。対話ループもここに置く（`run_with_settings` は `Terminal`、`WorkspaceLoader`、`SettingsPort` を注入し、Welcome ⇄ Open / New / Config、Open ⇄ Workspace の画面グラフを回す。Config は scope ごとの draft を持ち、保存失敗時も保持する。Workspace 内では Switch / Closeup の mode と Overview / PR の最前面 modal を状態機械で dispatch し、modal widget が組み立て済み workspace frame に枠を合成する。Recent は Welcome から Workspace へ直接進み、Esc で Welcome へ戻る） |
| `usecase/` | TUI に閉じた application ロジック。起動画面の `EntryScreen`、それを具体的な描画・入力実装へ委譲する `ScreenRunner` 境界、管理画面用の端末ポート `Terminal` と入力語彙 `Key`、live pane 専用の端末非依存入力語彙・bytes encoder・`Ctrl-O` classifier、Welcome / Open / Recent の typed attach と Home の純粋 controller（state / event / effect reducer、TUI-local backend port と fake backend）。controller が返した全 `Effect` を daemon 所有のポート群（session command / agent / notes・environment store / workspace command / decision / PR・preview・browser）へ振り分ける本番 executor `daemon_backend`。実 IO ポートは合成ルートが 1 つの backend factory から注入し、`effect → 実行 → event → update()` の単方向ループを閉じる。Home は runtime ごとの phase を保持し、target ごとに `done > waiting > running > ready > absent` で集約する。progress・operation / terminal error・disconnect / reconnect / resync は safe message と error ID だけを TUI-local feedback として保持する。stable `TerminalRef` で tab / pending placeholder / attach policy を扱う Closeup pane reducer と、その reducer を daemon inventory / stream / resume / geometry dedupe へ結合する `pane_runtime`、Overview / Closeup コマンドの解釈・dispatch、画面グラフの遷移、イベント処理の状態機械 |
| `infrastructure/` | daemon 端末へ attach する IPC クライアント側と端末バックエンド（raw mode・端末制御・キー/ホイール読み取り・クリップボード）。daemon push adapter は phase、safe error、connection feedback を TUI-local projection に変換し、wire の detail を越境させない |

`Terminal` は対話画面が使う端末の最小ポート（サイズ取得・フレーム描画・キー読み取り）で、`usecase` が
定義する。実端末の制御（crossterm による raw mode・代替スクリーン・入力 adapter・event pump）は
合成ルート（ルートの `src/main.rs`）だけが実装し、`usagi-tui` は crossterm に依存しない。pump は key の
kind / modifier / text、paste、resize を terminal 非依存入力語彙へ写し、terminal・backend・tick を controller
へ渡す単一の runtime stream に多重化する。これにより対話ループはフェイク端末で 100% ユニットテストでき、
実端末 IO は計測対象外の合成ルートに閉じる。実端末の Home フレームループ
（`presentation::drive_workspace_controller`）は、Home の row state・入力・描画を
controller に一本化する。state は `WorkspaceRuntime`（controller の `AppState` と
target-scoped `PaneRegistry`）が所有し、入力は `presentation::app_event_from_key` が
`Key` を controller の `AppEvent` 語彙へ写して（live prefix を解決済みの `Key::Live` は
対応する `AppKey` に、resize / backend wakeup の `Key::Other` は mascot を進める `Tick` に）
`update()` へ通し、描画は `HomeProjection` → `render_home` が生成する。sidebar の
pointer クリックは `Key::Click` を `AppEvent::Pointer`（座標＋種別）へ写し、reducer が
描画と同じ viewport 幾何で `Selection` へ hit-test して選択（single click）または
活性化（double click）する。shell は行の hit-test を持たず、double click の判定窓だけを
追跡する。terminal pane 内の drag / copy は Home 行契約と無関係なので shell +
`TerminalSession` に残る。前面の Pull Request
一覧・Markdown preview は controller の `Overlay::Prs` / `Overlay::Preview` が所有し、
素材は `Effect::LoadPullRequests` / `LoadPreview` で要求して
`BackendEvent::PullRequestsLoaded` / `PreviewLoaded`（失敗は対応する `*Error`）として
還流し、選択 PR の browser 起動は `Effect::OpenPullRequest` で表す。これらは他の overlay
と同じく `render_home` に統合し、shell が別途 modal を重ねる暫定接続は残さない（controller
に相当がある残りの create form・quit confirmation だけを shell が `render_home` の出力へ
合成する）。daemon IO（session worker・pane 起動・terminal stream・metrics）は runtime
shell が transport として担い、daemon-authoritative な session 一覧をキャッシュして毎フレーム
投影へ渡す。daemon metrics と session ごとの git diff は描画専用素材なので `AppState` に
載せず、shell が `MetricsBackend` で port を poll して各観測を drain し、`MetricsProjection`
キャッシュへ畳み込んでから `render_home` に渡す（`poll → drain → 反映 → 描画` の単方向）。
Home の row state・selection・入力・描画は controller が単独で所有し、旧 `Workspace` view の
二重定義は残さない。

production composition のデータフローは次の 1 経路だけである。direct workspace と
Welcome / Open / Recent / New の各入口は、workspace snapshot ごとに同じ
`ProductionBackendFactory` を呼ぶ。

```text
workspace entry
  → ProductionBackendFactory(snapshot)
  → DaemonBackend + daemon/store/platform ports + terminal host
  → controller Effect
  → DaemonBackend::dispatch（唯一の route table）
  → port の実 action または明示 error completion
  → AppEvent
  → controller update
```

session worker と terminal stream は stateful な接続を terminal host に保持するが、Effect を解釈しない。
`DaemonBackend` の host port が typed action を 1 件だけ渡し、host は session snapshot、create の token 付き
`OperationResult`、pane launch の完了を同じ completion loop へ返す。注入されていない機能を成功扱いする
no-op は置かず、安全な error completion にする。

**TUI の実装は core に吸収されない。** `usagi-core` が持つのは面をまたいで共有する
data（`domain/`）・IPC プロトコル型の定義・永続化・git（`infrastructure/`）と、
面をまたぐ共有ロジック（`usecase/`）だけである。描画・入力・画面遷移、および
attach の**クライアント側実装**は TUI に固有で、`usagi-tui` に置く。attach は
「プロトコル型は core・クライアント実装は tui」で分担する（daemon 側実装は
`usagi-daemon`）。したがって `usagi-tui` は core の薄いラッパではなく、
presentation 主体の実クレートになる。

## Markdown 永続化の commit contract

issue / memory store の永続化契約は本節を正本とする。source of truth は
`.usagi/issues/*.md` と `.usagi/memory/*.md` であり、`index.json` と memory の
`MEMORY.md` は source から破棄・再生成できる derived file である。derived file のための
durable source migration は行わない。

`index.json` の schema version は `2` で、sorted source file name（key identity）と source
Markdown の全 byte を長さ区切りで SHA-256 に入力した `source_fingerprint` を保持する。list は
current source set を同じ方法で fingerprint し、一致した index だけを採用する。このため rename、
同数の delete+add、mtime を保存した変更、粗い timestamp、same-size edit でも stale cache を返さない。
version が旧版・未知版、fingerprint が欠落・形式不正・未知 algorithm、または内容が不一致なら source
を走査して index を一度 rebuild する。rebuild は source Markdown を変更しない。

freshness check の cost は source file 数を `N`、総 byte 数を `B` とすると、directory scan と filename
sort が `O(N log N)`、read/hash が `O(B)`、作業 memory が 16 KiB buffer と path list の `O(N)` である。
fast path は全 source byte を各 1 回読む一方、frontmatter の parse と summary 再構築、derived file write
を省略する。この read/hash が cache hit の契約であり、mtime や file size だけを freshness identity として
利用しない。source mutation 後は一部 summary の patch ではなく全 source を走査して fingerprint と summary
を再生成し、外部編集された sibling の stale summary に新しい fingerprint を付けない。

mutation は store lock 下で次の順序に分ける。

```text
durable .derived-dirty marker
  → source Markdown の atomic write / remove（commit point）
  → derived index / TOC refresh
  → refresh 成功時だけ marker を削除
```

source commit 前の失敗は mutation 未適用の error である。source commit 後の derived refresh
失敗は source mutation の成功を取り消さず、`MutationOutcome` の `RebuildNeeded` として表す。
`.derived-dirty` は git 管理外の rebuild scheduling marker であり、次の get/list/search または store
reopen 後の最初の read が store lock 下で source を走査し、derived file を自己修復する。修復が
一時的に失敗しても marker を残し、source を直接読む操作は committed state を返す。

update / remove の再送は source key（issue number / memory name）に対して冪等である。issue
create の再送は、初期 status と request fields が一致する committed source を同じ store 内で
先に照合し、その番号が一意な同一 content identity なら既存番号を返す。matching source の番号が
重複していれば既存番号を任意に返さず ambiguity error になる。したがって derived failure や応答
消失の後に同じ mutation を再送しても、別番号の issue や二重削除を作らない。

issue number の採番 authority も本節を正本とする。Git repository では v1 / v2 が共有する
`<git-common-dir>/usagi/issue-numbers/`、非 Git workspace では
`<workspace>/.usagi/issue-numbers/` にだけ authoritative state を置く。

```text
<git-common-dir>/usagi/issue-numbers/
├── .lock
├── sequence.json                         # normal: { "version": 1, "last_reserved": N }
│                                         # blocker: last_reserved = u32::MAX,
│                                         #          migration_floor = F
├── legacy-v2-migrated                    # Git migration commit: canonical body "N\n"
└── reservations/
    └── 0000000516.reserved               # body: "516\n"

<git-common-dir>/usagi-issue-sequence/
├── .lock                                 # pre-fix v2 と共有する migration lock
└── next                                  # active: "N\n"
                                          # fenced: "migrated-to-usagi-issue-numbers:N\n"

<observed-issue-store>/usagi-issue-sequence/
├── .lock                                 # nested/non-Git の pre-fix store-local lock
└── next                                  # common legacy と同じ active / fenced format
```

raw cwd が repository 内の深い path でも、最寄り ancestor の `.git` まで遡って worktree boundary を決める。
authority は v1 と同じく、`GIT_DIR` / `GIT_WORK_TREE` / `GIT_INDEX_FILE` / `GIT_OBJECT_DIRECTORY` /
`GIT_COMMON_DIR` / `GIT_PREFIX` / `GIT_NAMESPACE` を除去した
`git -C <worktree-root> rev-parse --path-format=absolute --git-common-dir` の成功結果だけを canonical existing
directory として採用する。valid separate-git-dir / submodule で `commondir` が無い場合は Git が返す git dir
自体を使う。empty / non-repository `.git`、stale / dangling gitfile・`commondir`、non-UTF-8 / empty output、
その他の read error は local fallback directory を作らず fail-closed になる。

non-Gitで予約した後に`git init`するとauthority path自体が変わる。Git resolverはcaller / current
worktree / 登録済み全worktree rootの`<root>/.usagi/issue-numbers` が1つでもmaterialize済みなら、
abandoned journalを無視してGit-common authorityを作らずfail-closedにする。このtopology変更は異なるlock間で
atomicに切り替えられないため、cached non-Git allocatorを停止し、全fallback sequence / journal / source floorを
offlineでGit authorityへreconcileしてから旧fallbackを取り除く。absence checkだけはcached processをfenceしないので、
quiescenceは必須の外部gateである。Git→non-Gitへのclassification変更も同じoffline reconciliationなしで行わない。

lock 順序は new authority `.lock`、canonical parent identityでdedupした後の辞書順の列挙済み旧 v2 `.lock` の順で固定する。raw pathのsymlink aliasは同じ順序に正規化する。全 lock を保持したまま、
`sequence.json`、`legacy-v2-migrated`、全 reservation marker、旧 `next`、workspace root と
`.usagi/sessions/<name>/`、および登録済み全Git worktreeでtracked / untracked / ignoredとしてmaterialize済みの
arbitrary nested issue storeにある全sourceのfilename prefix / parse可能なfrontmatter宣言の最大値を最初に検証する。activeな旧`next`は
plain `u32` として high-water へ fold する。observed path のうち sentinel で封鎖されていない authority が2つ以上なら、
相互に独立した旧 writer を atomic に止められないため、authoritative file を書く前に停止する。

```text
fresh Normal sequence + sole unfenced legacy:
  v1-visible floor A == durable floor F:
    legacy next = sentinel(F)（atomic; 旧 v2 を最初に停止）
  sole live legacy floor B == durable floor F:
    sequence blocker(F)（atomic; 旧 v1 を一時停止）
  A < F and B < F:
    fail-closed（1 writeで安全にbridgeできない）
no unfenced legacy:
  sequence blocker(F)
pre-existing blocker + unfenced legacy:
  legacy next = sentinel(F)（旧 v1 は既に停止済み）

  → sequence blocker(F) を保証
  → all observed legacy next = "migrated-to-usagi-issue-numbers:N\n"
  → reservation marker
  → legacy-v2-migrated = "N\n"（Git のみ）
  → normal sequence.json
  → source Markdown
```

ここで `A` は全旧v1 callerが共有して見えるNormal sequence / reservation journalの最大、`B` はsole unfenced legacy
floor、`F` はこれらに全worktree source / blocker recovery floor / optional migration watermark / 全fenced legacy floorも加えたdurable最大、
`N = F + 1` である。異なる2 authority を1回でatomic updateできないため、fresh migrationの最初の成功writeは、もう片側に
全durable floorが見える場合にだけlive allocatorを1つへ減らす。sentinelは旧v2を恒久的にfenceし、blockerはnormal sequenceを
最後に戻すまで旧v1を一時停止する。両live sideがfenced watermarkより低ければ、どちらを先に止めても他方が番号を再利用するため、
write前にoffline reconciliationを要求する。source visibilityはcallerのworkspace rootによって異なるため、first-write判定で
`A`へ加えない。

blocker は `{ "version": 1, "last_reserved": 4294967295, "migration_floor": F }` を1回の atomic write で公開する。
旧 v1 は追加 field を無視するが `u32::MAX` の checked increment で停止し、fixed v2 は `F` から本来の high-water を
回復する。`migration_floor` が無い `last_reserved = u32::MAX` は最終番号を正常予約した exhausted state である。
blocker 以外で `migration_floor` が存在する、または floor 自体が `u32::MAX` の JSON は破損として拒否する。

旧 writer が先に列挙済み旧 lock を保持していれば、その writer が更新した最新 `u32` を fixed allocator が fold する。
fixed allocator を待っていた旧 v2 writer は sentinel を plain `u32` として parse できず fail-closed になる。non-exhaustedなnormal sequence
は全 sentinel / reservation / Git marker より後、かつ最後に公開するため、正常終了後は旧 v1も同じ authority の次番号へ
進める。Git の sentinel と `legacy-v2-migrated` は migration watermark であり通常採番の live high-water ではない。
通常予約では sequence / journal だけが進み、両 watermark は相互に一致した古い値のままでよい。後発 legacy path を
再移行するときだけ全 observed fence / marker を更新する。非 Git は migration marker を公開・更新しない。interrupted
development buildが残した既存markerだけをcanonical検証してrecovery floorへfoldし、legacy列挙を抑止する用途には使わない。

crash recovery は次の境界で固定する。atomic write の Write / Rename failure は old / new の完全な片方だけを露出する。

| 最後に durable になった境界 | crash 後に進める旧 allocator | retry |
| --- | --- | --- |
| first sentinel / blocker が未commit | 変更前の旧state（fixed予約なし） | 元のfloorからfirst writeを再試行 |
| sentinel-first(F), blockerなし | v1 のみ | v1のsequence / journal進捗をfold |
| blockerのみ | 高水位を持つsole unfenced legacy v2（存在時）のみ | その`next`の進捗をfold |
| blocker after sentinel-first, sentinel(N)なし | なし | blocker floorからF+1を予約 |
| first / partial / all sentinel(N) | なし | Nを消費し、retryはN+1へ進む |
| reservation marker | なし | journal を fold して次番号へ進む |
| Git migration marker | なし | blocker / journal / marker を fold |
| normal sequence, source 未作成 | v1 のみ（旧 v2 は fenced） | 予約済み gap を再利用しない |
| exhausted safe-first sentinel(MAX) | v1のみ（MAXで停止） | normal sequence(MAX)をrecovery tagとして公開 |
| exhausted safe-first sequence(MAX) | legacyはMAXで既に停止 | 全sentinel(MAX)へ収束 |
| exhausted normal(MAX) + partial sentinel / old Git marker | なし | 残りsentinel → marker(MAX) → final normal(MAX) |
| exhausted Git marker(MAX), final sequence failure | なし | blocker / journal / marker MAXをfoldしnormal(MAX)へ収束 |

sequence は strict な schema / version / blocker semantics、sentinel、migration marker、reservation marker は canonical な
filename/body を検証する。active legacy numeric だけは pre-fix parser と同じ trimmed `u32` を受理する。invalid state、read
failure、および non-exhausted normal sequence 下の Git marker / shared sentinel mismatch は新しい write より前に fail-closed になる。
marker 未作成の sentinel-first 境界、または blocker 下で crash が残した valid sentinel / reservation / marker floor の差だけは
最大値を fold して回復する。`Normal(u32::MAX)`は旧v1の停止をdurableに証明するterminal recovery tagなので、
その下でのshared sentinel(MAX) / 旧Git markerの差だけもpartial exhausted migrationとして回復する。

source high-waterはfilename prefixだけでなく、parse可能なfrontmatterの`number`も含む。
`007-*.md`が`number: 800`を宣言する場合も、prefixの無いsourceが`number: 900`を宣言する場合も、
宣言側を再採番しない。allocation時のsource read / parse failureは宣言high-waterを証明できないため
lenient listingのようにskipせずfail-closedにする。

durable floorが`u32::MAX`の場合も、safe first-write条件を満たすならallocation errorを即時返してnumericな旧`next`を残さない。
旧v1がMAXのsequence / journal / blockerを見る場合はsentinel(MAX)を先に公開し、sole legacyがMAXを持つ場合は
normal sequence(MAX)を先に公開する。旧v2が既にsentinelで停止している場合もsequence(MAX)を先に公開できる。
どちらのlive側もMAXを見ないsource-only exhaustionはwrite前に停止する。safeなfirst write後は
normal sequence(MAX)をrecovery tagとして直ちに公開し、全sentinel(MAX) → Git marker(MAX) →
final normal sequence(MAX)と収束する。reservationやsourceを追加せずexhaustion errorを返す。

Git は common legacy を常時、normalized worktree より深い caller の current store-local path を `next` 未作成でも列挙する。
その他の Git root / workspace / direct-session path は legacy directory が存在するとき列挙し、さらに登録済み全 Git
worktree の tracked / untracked / ignored file から materialize 済みの nested legacy `next` を pathspec で毎回発見する。
同じauthority lock下のdiscovery phaseでnested issue Markdownを発見し、そのstore-local legacy pathも`next`未作成の段階からlock / fence
対象へ加える。registered worktree root自身は旧v2も`.git`経由でcommon legacyを使うためstore-local pathを追加しない。
非 Git は workspace root / current と存在する全 direct-session root を `next` 未作成でも列挙する。`Active` と `Missing` は
ともに未封鎖で、列挙済み authority のうち2件以上なら blocker 前に停止する。dangling sessions / issue store / authority pathも
「missing」と推測せず、session symlinkも暗黙に追わずfail-closedになる。非 Git は global completion marker を公開しないため、後発 direct session を
既存 marker で隠さない。

ただし、Gitでまだ legacy fileを作っていない arbitrary nested cwd、非Gitのroot/current/direct-session外の
arbitrary nested cwd、および列挙snapshot後に初めてmaterializeするpathは有限に封鎖できない。sentinel markerだけで
その未知 process の将来 write を防げるとは主張しない。最初の fixed reservation 前に全
pre-fix process の cwd / legacy path を inventory し、列挙対象へ materializeして fenceするか停止して再起動を禁止することが
safe rollout の外部 compatibility gate である。複数の未封鎖 file が見つかった場合は、全 pre-fix writer を停止し、最大
floor を失わず1つの authorityへオフラインで整理するまで allocationを再開しない。
並行回帰では、HEAD直前のraw-cwd resolver / filename floor / plain-u32 increment / StoreLockと同じロジックを
別processで実行するold-v2 compatibility emulatorを用いる。これはhistorical binaryそのものとは呼ばず、
旧側先行予約のfoldとfixed側先行sentinel parse failureを実OS process / file lockで固定する互換fixtureである。
release acceptanceではこれと分けて、pre-fix commit `677405d31267e9205b76a26fe8b31098b6086852`からbuildした
実`usagi 2.6.0` MCPを同一processのまま維持するrollout試験も行う。旧MCP create、fixed MCP createによるfold / fence、
同じ旧MCP processの再createという順で、最後がsentinelのplain-`u32` parse errorになり、source / derived / authority
artifactがbyte-for-byte不変であることを確認する。この実binary試験はrelease時の証拠であり、CIで常時動かす
deterministicなlock / crash境界の保証は上記emulator回帰が担う。

issue number は番号指定 CRUD の identity である。同じ番号 prefix の source Markdown が複数ある場合、
point get / update / delete と同番号への write は、番号と衝突した全 exact path を辞書順で保持する typed
`AmbiguousIssueNumber` error で fail-closed になる。point read は store lock を取得してから一意性を判定し、
選んだ exact path の read と必要な derived repair が終わるまで同じ lock を保持する。検査は dirty marker、
source write、source remove、derived repair より前に行うため、ambiguity error 後は全 sibling と derived state が
不変のまま残る。filename prefix と parse 可能な frontmatter の `number` が一致しない source も typed error とし、
point operation はどちらを正しい identity とも推測しない。自動 renumber や自動削除は repair として行わない。

list / search は source set の診断面でもあるため、同番号 sibling を collapse せず parse 可能な Markdown ごとの
row として exact filename 付きで列挙し続ける。parse 不能な sibling も filename prefix により衝突数へ含めるので、
残った parse 可能な search row は `ambiguous: true` と `ready: false` になり、重複番号を参照する依存もその番号を
`unmet_deps` に残す。search は store lock 下の 1 回の directory enumeration から raw filename claim と
parse 可能な exact-path source を同時に得て、query、ambiguity、done dependency、readiness の全判断に同じ
snapshot を使う。snapshot 前には同じ lock のまま scheduled derived repair を試みるため、自己修復契約も保つ。
全文 query は番号集合でなく source ごとに照合する。重複を修復するときは ambiguity
error が示す exact path ごとに git 履歴と参照元を監査し、残す identity と新番号へ移す identity を明示的に
決める。番号指定 delete は repair 手段に使わない。

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
- IPC の frame codec、surface-neutral な byte-stream `Connection` port、queue 上限は
  `usagi-core` に置く。Unix domain socket の bind/connect、owner・symlink・peer UID
  検証、generation locator は `usagi-daemon` の infrastructure adapter に置く。これにより
  TUI の attach 状態機械は socket 実装を参照せず、実 socket を束ねるのは合成ルートだけである。
- managed session の lifecycle state は `usagi-core::domain::session_lifecycle` の pure reducer と
  `infrastructure::store::lifecycle::DaemonLifecycleStore` に分ける。後者を保持して reducer 結果を永続化するのは daemon の command handler
  だけであり、TUI / CLI / MCP は IPC command を通じて要求する。legacy `state.json` は incarnation を持たないため、通常運用では
  managed state として解釈しない。ただし shared lifecycle state の初期化時だけ、daemon が worktree と repository binding を検証した全 record を
  stable ID 付き available session として一回だけ採用する。既存 shared state に対する adoption は daemon IPC の明示 recovery action だけが実行し、restart や UI refresh は実行しない。UI-only metadata は legacy store に残し、TUI が同名 record へ読み取り結合する。
- supervisor run の durable state は `usagi-core::domain::supervisor` の pure reducer と
  `infrastructure::store::supervisor::SupervisorStore` に分ける。store は daemon state dir に atomic snapshot と append-only event journal を保持し、
  lock と state revision CAS で書き手を fence する。query は task instruction 本文、secret、raw runtime argv を返さない。scheduler と policy はこの state の
  event producer であり、domain/store はそれらを解釈しない。
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

配布物は従来どおり**単一バイナリ `usagi`** のまま。合成ルートはすべての起動で完全な
process argv を `runtime::cli::dispatch` へ渡し、その解析結果から面を選ぶ。

| 起動 | 面 |
|---|---|
| `usagi`（引数なし） | TUI 面（`usagi-tui`） |
| `usagi daemon` | daemon 面（`usagi-daemon`） |
| `usagi mcp` | 入口面の MCP（`usagi-cli` の `mcp/`） |
| その他のサブコマンド | 入口面の CLI（`usagi-cli` の `cli/`）。実行結果の TUI 要求は合成ルートが TUI 面へ委譲 |

個々のコマンドと起動面の対応は [1. プロジェクト概要#現在の実装状態](01-overview.md#現在の実装状態)が
正本である。

### process argv contract

本節が、単一バイナリの **process argv の文法・parse-before-effect・終了 status** の正本である。
合成ルートはバイナリ名を含む完全な argv を `usagi-cli` の単一 top-level clap command tree へ渡す。
この tree が全 tail を検証して typed `RunOutcome` route を返し、合成ルートはその後にだけ実行面へ
dispatch する。第 1・第 2 引数を先に手動判定して残りを捨てる経路は持たない。引数なしの Welcome
TUI、通常の CLI、daemon lifecycle、MCP stdio server はすべてこの境界を通る。

```text
process argv
     |
     v
合成ルートの runtime::cli::dispatch
     |
     v
usagi-cli の単一 top-level clap tree で完全解析
     |
     +-- usage error ----------------------> stderr: error + usage
     |                                      stdout: empty / ExitCode 2
     |                                      effect: none
     |
     +-- --help / --version --------------> clap output / ExitCode 0
     |                                      runtime effect: none
     |
     `-- typed RunOutcome route
             |
             +-- 引数なし ----------------> Welcome TUI
             +-- 通常 CLI ----------------> handler / TUI request / daemon IPC
             +-- daemon ------------------> lifecycle handler
             `-- mcp ---------------------> daemon bootstrap -> stdio serve loop
```

解析が成功するまで、TUI の端末初期化、CLI handler、daemon の record / lock / process / signal /
socket 操作、MCP の daemon bootstrap・autostart・stdio serve loop を開始しない。とくに
未知 verb の `usagi daemon bogus`、有効 verb と余剰引数の `usagi daemon status extra`、
余剰引数付きの `usagi mcp extra` はすべて clap の usage error である。これらは exit 2、
stderr の error と usage、空の stdout で終了し、daemon や MCP の side effect を起こさない。

解析済み argv の実行中に起きる failure は usage error と区別する。daemon endpoint の接続・
transport failure、protocol rejection、application rejection、MCP の daemon bootstrap failure などは
exit 1 とし、stdout を空に保って stderr に safe user message（該当時は machine-readable な
error code / `error_id`）を出す。正常な CLI、daemon lifecycle、MCP stdio の挙動は解析成功後の
各 route が担う。

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
| coverage | `cargo llvm-cov --workspace` で crates/ 配下も計測される。計測から外す item はソースコードの `#[coverage(off)]` で明示する |
| test / clippy | ルートで実行するとルートパッケージしか対象にならないため、`--workspace` を付ける（test.yml / lefthook / recommend-tests の fail-safe も同様） |
| auto-release | リリース起点はまだ v1（`v1/Cargo.toml` の version を監視）。ルートにはリテラル `version` を維持しておき、v2 初リリース時に監視対象をルートへ切り替える |
| release-build-check / release.yml | まだ v1 を対象に release ビルドする（`--manifest-path v1/Cargo.toml`）。v2 初リリース時に root bin のビルドへ切り替える |
| `v1/` | `[workspace] exclude` で計測・ビルド対象外。`v1/**` を変更する push / PR は v1-test.yml が v1 のマニフェストで検証する |

## 実装の置き場所ガイド

v1 から機能を再実装するときの置き場所の指針。

| 実装 | 置き場所 |
|---|---|
| `Workspace` / `Settings` / `Issue` などのエンティティ、および画面が並べて見せる読み取り値（`WorkspaceOverview` = workspace＋各カウント、`UniteOverview` = 合併した workspace 群の合計、welcome 画面の recent 一覧が持つ `Recent` = そのどちらか） | `crates/core/src/domain/` |
| agent の static profile、product-neutral capability、immutable launch request / plan / durable snapshot | `crates/core/src/domain/agent/`。CLI 文法・shell rendering・PTY・secret・provisioning は置かない |
| `state.json` などの store・IPC プロトコル型・git 操作 | `crates/core/src/infrastructure/` |
| workspace の登録・touch・recent overview 構築、セッション作成・設定解決など両面が使うロジック | `crates/core/src/usecase/` |
| profile catalog seam と profile/request・durable snapshot の pure validation | `crates/core/src/usecase/agent.rs`。catalog は adapter が code-defined descriptor を登録する境界であり、durable state の正本ではない |
| product 固有 agent adapter と scoped materialization | `crates/daemon/src/usecase/runtime.rs` の `AgentAdapter` / `SpawnProvision`。adapter は reservation 前に durable snapshot と非永続 spawn provision を一度だけ組み立てる |
| Codex profile の argv renderer と config / MCP / hook の materialization | `crates/daemon/src/usecase/codex/`。Codex adapter は共通 `AgentAdapter` を実装し、secret の値・一時 config 引数を `SpawnProvision` だけへ渡す |
| PTY 所有・IPC socket サーバ・daemon 永続化（daemon 専用の外部接続） | `crates/daemon/` の `infrastructure/` |
| セッション監視ティック・autostart queue consumer・通知調停（daemon 専用ロジック） | `crates/daemon/` の `usecase/` |
| IPC リクエストの dispatch・応答整形（daemon サーバ入口） | `crates/daemon/` の `presentation/` |
| 各画面の描画（view） | `crates/tui/` の `presentation/views/` |
| 画面をまたぐ再利用 UI 部品（widget） | `crates/tui/` の `presentation/widgets/` |
| 色（意味的な役割→具体色）・色定数 | `crates/tui/` の `presentation/theme` |
| 領域配置・ペイン分割・chrome（layout）。マスコットを頂く全画面の共通 chrome は `layouts/mascot_screen`（welcome / config が共有） | `crates/tui/` の `presentation/layouts/` |
| 画面グラフの遷移・イベント状態機械 | `crates/tui/` の `usecase/`。Entry / New / Home controller は `usecase/application/controller.rs` で state / event / effect を純粋に還元する。Welcome / Open / Recent が選んだ `WorkspaceId` は attach completion と照合し、同じ ID の Home snapshot だけで Home を初期化する。New は Clone の git→project/registry と Existing の project/registry 登録を TUI-local backend port へ effect として渡し、operation token で completion を照合する。validation / backend failure は form を保持したまま同じ request を retry でき、成功時だけ Home を初期化する。遅延・不一致の completion は無視し、daemon wire は backend port の外へ閉じる |
| Closeup terminal / Agent tab の placeholder・選択維持・attach policy | `crates/tui/` の `usecase/application/pane.rs`。pure reducer は stable `TerminalRef` と `OperationId` だけを受け取り、`pane_runtime.rs` が daemon inventory / stream、resume / resync、input、geometry dedupe と client-only detach を結合する。`AgentRuntime` は controller が発行した `LaunchAgent` effect だけを session-scoped host へ渡し、profile、operation、terminal identity を表示名や local process に置き換えない |
| Overview コマンドの解釈・dispatch | `crates/tui/` の `usecase/overview/`（ハンドラは `overview/commands/`） |
| Closeup コマンドの解釈・dispatch | `crates/tui/` の `usecase/closeup/`（ハンドラは `closeup/commands/`） |
| CLI から選ばれた TUI 起動画面の dispatch | `crates/tui/` の `usecase/application.rs`（CLI 要求からの変換はルート `src/main.rs`） |
| attach クライアント・端末バックエンド | `crates/tui/` の `infrastructure/` |
| CLI サブコマンドの引数解析・dispatch・結果整形 | `crates/cli/` の `cli/`（ハンドラは `cli/commands/`） |
| MCP サーバ（JSON-RPC の解釈・dispatch・tool アダプタ） | `crates/cli/` の `mcp/`（アダプタは `mcp/tools/`） |
| 各面への dispatch と実 IO の注入 | ルート `src/`（実 IO の注入のみ。テスト可能なロジックは crates へ） |
| macOS LaunchAgent の plist 供給・load / unload | ルート `src/runtime/launchd.rs`。launchd は前景 `daemon serve` の process supervision のみを担い、daemon lock や session state の権威を持たない |

### Agent launch boundary

agent 起動の core 契約は `AgentProfile`、`AgentCapability`、`LaunchRequest`、`LaunchPlan` と
`DurableLaunchSnapshot` である。`AgentProfileId` は static profile の stable ID、`AgentRuntimeId`
は 1 回の process runtime の incarnation であり、同一視しない。agent capability も IPC の
negotiation capability、terminal authorization、lifecycle capability とは別の closed vocabulary である。

`LaunchRequest` は profile 選択・mode・model selector・resume・prompt・scope・必要 capability
だけを表す。adapter が一度だけ renderer で得る `LaunchPlan` は shell command string ではなく
`program` と `argv` を持つ。environment は名前の allowlist だけを durable に扱い、値・secret・
adapter private config は含めない。

daemon が snapshot を再生するときは schema と profile revision を検証する。不一致、unknown profile、
request capability 不足、plan provenance 不一致は typed error で fail-closed とし、最新 catalog から
黙って別の意味へ再解決しない。実 executable 検査、設定 materialization、secret 注入、PTY spawn は
adapter / daemon infrastructure の責務である。

Codex / Claude adapter は daemon の terminal launch 子層である `usecase::codex` / `usecase::claude` に閉じる。各 product の CLI flag、
model の解釈、config / MCP / hook の payload はそれぞれの provisioner 内部だけが扱う。adapter は共通の
`AgentAdapter` として reservation 前に durable snapshot と `SpawnProvision` を組み立て、runtime は snapshot を
保存してから provision を PTY spawner へ一度だけ渡す。`SpawnProvision` は durable record、IPC、terminal
stream、error detail に残らない。

durable snapshot が持てるのは `program`、`argv`、working directory、環境変数**名**の allowlist だけである。
credential、secret、raw hook payload、provisioned file path は `SpawnProvision` にだけ存在し、保存・event・
error detail に載せない。

### Daemon runtime ownership

daemon の `usecase::runtime::RuntimeCoordinator` は、一回の Agent runtime を所有する。
`LaunchResolver` は request ごとに一度だけ呼ばれ、返した `DurableLaunchSnapshot` と
`AgentRuntimeRef`、`CompletionFence` を `RuntimeStore` へ保存してから injected `PtySpawner` を
呼ぶ。restart/reconcile は保存済み snapshot を対象にし、profile を再解決しない。

raw output は `OutputJournal` へ保存してから既存の `TerminalRegistry` の replay に公開する。
detach/disconnect は attachment だけを外し、PTY と runtime reservation を停止しない。spawn 応答の
欠落、spawn 後の永続化失敗、process identity unknown、verified-alive orphan は typed
`ReconcileRequired` となり、replacement spawn と concurrency slot の解放を止める。slot を解放するのは
final output を drain 済みの verified exit、または identity を伴う reconcile が `Gone` を確認した場合だけである。

product 固有 adapter、secret、IPC schema はこの coordinator の境界外である。

daemon の production composition は store、PTY、journal、writer、session scope、record publisher を trait object の
port として同じ constructor に注入する。テスト専用 constructor や別実装の wiring は持たず、production が使う
composition を fake port / failpoint で直接通す。store failpoint は reservation 後の partial effect、restart hydrate、
stale ref、observer exit の ordering を検証し、実 socket accept・OS signal・PTY syscall だけを理由付き
`coverage(off)` と integration test の対象にする。

daemon は journal に commit 済みの PTY output から HTTP(S) の `github.com/<owner>/<repo>/pull/<number>`
だけを検出し、suffix・query・fragment を除いた canonical URL を stable `SessionId` ごとの PR inventory
へ投影する。inventory は daemon data directory の atomically replaced JSON snapshot であり、terminal ID、
worktree path、TUI selection を identity に使わない。検出は増分で行い、chunk / UTF-8 境界をまたぐ候補も扱う。
credential・control character・不正 percent encoding・非 GitHub host・0 または overflow の番号は fail-closed
で除外する。再検出は revision を増やさず、ユーザーが pin または dismiss した entry を復活・上書きしない。
IPC wire、`gh` enrichment、TUI 表示はこの projection を読む後続の面の責務である。

agent runtime と generic shell の terminal lifecycle は `usecase::terminal` が正本である。両者は
`TerminalRuntimeState`、`TerminalReconcileState`、`SpawnFailure` と `TerminalRegistry` を共通で使う。
違いは terminal を起動する前段だけで、Claude/Codex は terminal launch 子層の adapter、generic shell は
trusted terminal profile resolver として program/cwd/env を解決する。いずれも reservation 後の detach、replay、
verified exit、reclaim を独自実装しない。

### Generic terminal launch boundary

通常の interactive terminal は Agent runtime ではない。client は `TerminalProfileId` と登録済み
workspace / session / worktree scope、geometry だけを `TerminalLaunchRequest` として送る。raw shell
command、argv、cwd、env は request / IPC に置かない。daemon は `SessionRuntime` の available managed
session resolver で request の完全な workspace / session / worktree fence を検証し、その worktree path を
`TerminalProfileResolver` へ渡して code-defined profile または trusted local settings から一度だけ program、cwd、
非 secret env を解決する。不一致・未利用可能な scope は PTY spawn 前に拒否する。

`GenericTerminalCoordinator` は `TerminalRef` と `CompletionFence`、profile revision、program、cwd、env
**名**の allowlist だけを `TerminalStore` へ保存してから PTY を spawn する。env の値、secret、rendered
shell command は durable record、IPC event、通常 log に保存しない。generic terminal は
`AgentRuntimeId`、AgentProfile、phase token を作らず、agent hook / MCP injection / adapter provisioning を
呼ばない。

attach / detach / replay / verified exit / reclaim は既存 `TerminalRegistry` と #251 の reservation contract を
使う。disconnect は attachment だけを外して PTY を生存させる。同じ durable record の identity unknown、orphan、
ambiguous spawn は replacement spawn と slot release を block し、verified exit または `Gone` の reclaim だけが
slot を解放する。一方、現行 generic Terminal Launch は producer `OperationId` を wire に持たず、daemon が request
ごとに terminal / operation identity を新規発行する。spawn 後の response が失われて client が launch を再送すると
別 record として二重 spawn し得るため、response-loss idempotency と cross-generation capacity は
[#518](../.usagi/issues/518-refactor-daemon-owner-generation-runtime-shard-global-resource-allocator.md) で追跡する。

### Agent orchestration の fence

`usecase::orchestration::AdapterRegistry` は Claude と Codex を同じ typed orchestration port に登録する。
daemon は profile ID によって registry を引くだけで、product 名による lifecycle・authorization 分岐を持たない。
MCP wiring は profile の `McpWiring` capability と、別個の workspace/session authorization の両方が通った launch
だけで adapter の scoped provisioner に要求する。provision failure は spawn 前に typed error として止まり、secret・
raw config・rendered argv は durable record、IPC、terminal annotation、log に渡さない。

phase hook は daemon が生成した一 runtime 限定 token、完全な `AgentRuntimeRef`、`CompletionFence`、単調 source
sequence を照合してから in-memory projection に反映する。token と hook payload は永続化しないため、restart 後や
foreign / duplicate / exited runtime の report は fail-closed で無視または拒否される。resume は immutable snapshot の
schema、request/plan provenance、profile revision と capability を再検証するだけで再 renderer / spawn しない。reclaim は
verified process identity の disappearance または orphan だけを記録し、unknown identity、ambiguous spawn、secret loss は
明示 action を必要とし replacement spawn をしない。

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
- **session lifecycle 接続**: controller は `Command::Session` の未解釈引数だけを typed
  `SessionCommand` に再解釈し、`CreateSession` / `RefreshSessions` /
  `RemoveSession` effect を発行する。adapter が daemon wire 型へ変換するため、TUI は store・git・PTY や
  daemon IPC を直接操作しない。削除対象は表示名でなく active target の stable `SessionId` である。runtime は
  起動時と session command の accepted/final 後に daemon の lifecycle snapshot を再取得し、sidebar/Overview の
  read-only projection を置換する。この projection を legacy `WorkspaceState` へ書き戻さず、reconnect/reload は
  snapshot と operation replay の barrier から再構成する。

## TUI Closeup のコマンド dispatch

`crates/tui` の `usecase/closeup/` は、Closeup 固有のコマンド語彙
（`agent` / `close` / `diff` / `terminal`）を持つ。`interpret` は入力を
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

`crates/cli` の `cli/` は、process 全体の top-level clap command tree と、通常 CLI コマンド面の
骨格（枠）を持つ。tree は引数なし・通常 CLI・daemon・MCP の完全な argv を解析し、各実行面を表す
typed `RunOutcome` route を返す。通常 CLI の handler としてここに置くのは、**ターミナルから
`usagi <cmd>` で叩く人間向けコマンド**（`open` / `config` / `doctor` / `update` / `completion` /
`version` と clap 自動の `help`）である。エージェント向けの issue / memory 操作は MCP 面
（`crates/cli/mcp/`）が受け持ち、daemon / MCP route の effect はそれぞれの実行面が担う。
どんなコマンド・オプションがあるかは単一の clap command tree から `usagi --help` と型の両方に
反映される。通常 CLI branch の dispatch は `Run` トレイトで一様化する。

```text
解析済み CLI Command ─► Command::into_handler() ─► Box<dyn Run> ─► Run::run(out)
                                                                        │
                                                           ┌─► Exit(code) ────────────┐
                                                           ├─► LaunchTui(request) ─► TUI ─┤
                                                           └─► DaemonRequest ─► IPC ─────┤
                                                                        合成ルート ─► ExitCode
```

- **`Run` トレイト**: 各コマンドの実行を `fn run(&self, out) -> io::Result<RunOutcome>` に一様化する。
  巨大な match ではなく、コマンドごとのハンドラ型が `Run` を実装する。ハンドラは
  **1 コマンド = 1 ファイル**（`cli/commands/<command>.rs`）に置く。
- **dispatch**: `Command::into_handler` が解析済みコマンドを対応ハンドラに変換する 1 か所の対応付け。
- **エントリ `run(args, version, out, err)`**: 実 IO を注入して受け取り、引数なしの TUI 起動、
  通常 CLI の終了・TUI / daemon 要求、daemon lifecycle、MCP stdio server の typed route を
  `RunOutcome` で返す。`TuiRequest` は CLI 側の意図であり、合成ルートが TUI 側の
  `EntryScreen` へ変換するため、面クレートどうしは依存しない。
  `args` は単相化を増やさないよう `Vec<OsString>` の具体型で受ける。配布 version は
  ルートパッケージだけが持つ（[単一バイナリと合成ルート](#単一バイナリと合成ルート)）ため、
  `--version` の値は合成ルートから注入し、clap コマンドに載せる。process 全体の解析境界は
  [process argv contract](#process-argv-contract) に従う。
- **終了 status contract**: 合成ルートは `RunOutcome` と daemon の typed reply を process
  `ExitCode` に変換する。CLI 自身の正常完了、daemon の `Ok`、契約上の `Accepted` だけが 0 である。
  daemon endpoint の接続・transport failure、protocol rejection、application rejection は 1 である。
  usage error の exit 2 と effect 境界は [process argv contract](#process-argv-contract) を正本とする。
  daemon failure は stdout を空に保ち、stderr に safe user message と
  machine-readable な error code（protocol error では `error_id` も）を出す。library / usecase は
  `process::exit` を呼ばない。従来 daemon error 時の 0 を前提にした automation は非 0 を扱う必要がある。
- TUI 起動要求を返すコマンドの対応は
  [1. プロジェクト概要#現在の実装状態](01-overview.md#現在の実装状態)が正本である。
- **`completion`** は実装済み: `clap_complete` で `Cli` のコマンドツリーから対象シェル
  （`clap_complete::Shell`）の補完スクリプトを生成して標準出力へ出す。定義が唯一の真実なので
  補完候補は CLI の実態と一致する。ただし静的ジェネレータの仕様上、`hide = true` の内部コマンド
  （`hop` / `mcp` / `daemon serve` / `agent-phase` / `guard-workspace`）も補完候補には含まれる
  （`--help` には出ない）。
- **`update`** は実装済み: 通常は最新 GitHub Release を、`usagi update -v` では5行固定の一覧から `↑` / `↓` で選んで Enter で決定した release を、platform 固有 archive と SHA-256・release version artifact を使って
  `scripts/install.sh` 経由で mode 0700 の private staging へ download する。installer は archive が path traversal /
  symlink / unexpected entry を含まず単一の通常ファイル `usagi` だけを持つこと、checksum、candidate version を検証する。
  更新全体を user-local lock で直列化し、検証後に `~/.usagi/bin` と同じ filesystem 上の atomic rename で置換するため、
  途中失敗では旧 binary の bytes と mode が変わらない。CLI は trusted な installer command の要求だけを返し、network /
  subprocess の実 IO は合成ルートが実行する。installer は inherited CWD の binary を参照せず、検証 artifact のない旧 release
  へ fallback しない。更新後のバイナリは次回の `usagi` 起動から使われる。
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

dispatch MCP の正本は本節である。`session_dispatch`、`session_get`、`agent_list`、`agent_get`、
`agent_complete`、`agent_fail`、`agent_inbox` は tool schema と daemon IPC request 型を公開する。
daemon は private caller credential を live runtime と照合し、session lifecycle、Agent runtime、dispatch
store と caller inbox を一つの durable 経路として compose する。credential の無い呼び出しや current run と
一致しない完了報告は fail-closed で拒否し、payload の caller identity は信用しない。

`session_dispatch` の新規 agent は workspace の `.usagi/config.toml` にある
`[agents.claude].models` / `[agents.codex].models` allowlist だけから選ぶ。MCP server は起動時に
allowlist と PATH 上の `claude` / `codex` の存在を snapshot し、非空 allowlist と executable の
両方を持つ runtime だけを `tools/list` の `agent.runtime` / `agent.model` enum に載せる。既存 agent は
`agent.id` branch を使い、runtime/model branch とは JSON Schema `oneOf` で排他的である。snapshot は
server lifetime 中は変わらないため、設定、PATH、CLI install/uninstall の変更を反映するには MCP server の
再起動または client 再接続が必要である。allowlist の正本や schema を CLI/provider の model list で拡張・保存しない。

既存の `session_create` と `session_delegate_issue` は移行期間の `agent_cli` alias を受ける。parser は
これを `runtime` に正規化するが、`runtime` または `agent.id` と混在した alias は migration error として
拒否する。`session_delegate_brief` は `session_dispatch` と同じ必須 `agent` selector を使い、
`agent.id` と `runtime`/`model` の混在を拒否する。認証済み caller の provenance を dispatch binding に
保存し、隔離 worktree 内の worker を直ちに起動する。

既存の `session_delegate_brief`、`session_delegate_issue`、`issue_to_prompt`、`session_prompt`、
`session_complete` は引き続き利用できる。

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
- **レジストリと dispatch**: `tools::registry()` が全 tool を連結し、MCP serve は Global / Workspace の実効設定で
  issue / memory 系統を filter した同じ descriptor 集合を listing と call に使う。`mcp::dispatch(name, params)` は
  名前で引いて `call` を呼ぶ。CLI の `Command::into_handler` に対応する一様な経路。
- **serve ループ**（`mcp/serve.rs`）: stdio 上の JSON-RPC 2.0 を 1 行ずつ処理する。純粋な
  ルーティング（`handle_line`: str → 応答 or 通知の無応答）と実 IO の反復（`serve`）を分け、
  応答エンベロープの整形は `mcp/protocol.rs` に集約する。`initialize` と `tools/list` は実際に
  応答し、`tools/call` は tool を名前で引いて store または daemon の実行経路へ送る。tool または
  daemon の失敗は JSON-RPC エラーに変換する。配布 version は合成ルートが `serve` に注入する。
- CLI のコマンドハンドラと MCP の tool は **同じ core usecase を呼ぶ兄弟**で、共有ロジックは
  すべて `usagi-core` に置く（[入口面 CLI のコマンド dispatch](#入口面-cli-のコマンド-dispatch)）。

## 検討した代替案

構成を決めたときの設計判断の記録。

| 代替案 | 不採用の理由 |
|---|---|
| 単一クレート内のモジュール分割（v1 方式） | 面・層の依存方向をコンパイラで強制できない。ビルド・テストのクレート単位並列性も得られない |
| 層ごとのクレート分割（domain / usecase / infrastructure / presentation を各クレート化） | 実行面（TUI / daemon）の境界を表現できず、daemon 専用と TUI 専用の infrastructure が同じクレートに同居する |
| TUI / daemon を別バイナリとして配布 | リリース CI（4 プラットフォーム）と配布手順の変更が大きい。単一バイナリ＋サブコマンドなら現行リリース機構が無変更で使える |
