# 1. プロジェクト概要

> [ドキュメント目次](README.md) ｜ 次へ → [2. アーキテクチャ](02-architecture.md)

## 目次

- [usagi とは](#usagi-とは)
- [v2 の位置づけ](#v2-の位置づけ)
- [v1 との関係](#v1-との関係)
- [現在の実装状態](#現在の実装状態)

## usagi とは

`usagi` はセッション・worktree オーケストレータである。リポジトリごとに隔離された
worktree（セッション）を作り、複数の AI エージェント・シェルを並行して走らせ、
issue の委譲から PR の作成・マージまでのループを回す。

## v2 の位置づけ

v2 は usagi のフルリライトである。v1 で決定した「PTY 所有を daemon に移し、TUI は
daemon が所有する端末に attach するクライアントになる」設計
（[v1/document/proposals/02-daemon.md](../v1/document/04-orchestration.md)）を
最初から前提にした構造で作り直す。コードの構成は
[2. アーキテクチャ](02-architecture.md) を正本とする。

## v1 との関係

| 場所 | 内容 |
|---|---|
| `/`（ルート） | v2 の実装。ビルド・CI（fmt / clippy / test / coverage 100%）の対象 |
| `v1/` | 退避した旧実装。仕様ドキュメント（`v1/document/`）ごと独立した Cargo プロジェクトで、ルートの workspace から exclude されている |

- **出荷するのはルートの v2 パッケージ**である。リリースはルート `Cargo.toml` の version 変更を起点に
  自動化されており、v1 はリリース経路に乗らない（[6. 開発規約#リリース](06-conventions.md#リリース)）。
- v2 として最初に出す version は既存の v1 release（`2.9.1`）より大きくする必要がある。小さいと
  `/releases/latest` が v1 のままになり、installer が新しい v2 を選ばない。
- v1 は `v1/` 配下で従来どおり単体ビルドでき、tree に残る間は `v1-test.yml` / `v1-coverage.yml` が検証する。

## 現在の実装状態

v2 は workspace の骨組み（[2. アーキテクチャ](02-architecture.md)）と、それを検証する
最小の実行面を持つ。CLI が TUI の起動要求を返し、合成ルートが TUI の初期画面へ
変換するため、入口面と TUI 面のクレート間に直接依存は生じない。以下の表が
コマンドから起動面への対応の正本である。

| コマンド | 動作 |
|---|---|
| `usagi` / `usagi hop` | Welcome 画面を対話的に表示する（Open で workspace 一覧へ、`1`〜`3` で Recent を直接開く、New で新規作成フォームへ、Config で設定画面へ進む） |
| `usagi open [path]` | `path` の workspace を登録・最終利用日時更新して Workspace 画面を開く。`path` 省略時はカレントディレクトリを使う |
| `usagi config` | Config 画面を対話的に表示する（scope を Tab で切替、`↑↓` で項目を選び `←→` で変更、dirty 時だけ Save で保存。`Esc` で Welcome へ、`Ctrl-C` で終了） |
| `usagi doctor` | Doctor TUI を起動画面に選ぶ |
| `usagi update` | GitHub Releases の最新バイナリを download して `~/.usagi/bin/` へ導入する。`usagi update -v` は release 一覧を5行固定で表示し、`↑` / `↓` で選択して Enter で選んだ版を導入する。反映には再起動が必要 |
| `usagi version` / `usagi --version` | 配布 version を表示する |
| `usagi daemon start` | daemon をバックグラウンドで起動し、登録された pid を表示する。すでに稼働中ならその pid を表示する |
| `usagi daemon stop` | 稼働中の daemon を終了する。live な Agent / generic terminal を持つ daemon は拒否し、`--force` で明示的に手放したときだけ停止する。stale な lifecycle record は回収する |
| `usagi daemon status` | daemon が稼働中か、stale record が回収可能かを表示する |
| `usagi daemon restart` | 稼働中 daemon を入れ替える。live runtime が無ければ cold transition、あれば通常は PTY を維持する seamless rollover を行い、安全な handoff の前提が欠ける場合だけ拒否する。`--force` は live PTY を明示的に破棄する cold transition（[planned replacement](05-daemon.md#planned-replacement)） |
| `usagi daemon` | daemon を前景で serve する（通常は `start` が起動する内部経路） |
| `usagi mcp` | daemon へ接続し（停止中は自動起動）、接続後は stdin の EOF まで stdio JSON-RPC server を実行する。daemon に接続できなければ server を開始せず failure status で終了する（[MCP の起動と経路](07-mcp.md#起動と経路)） |
| `usagi <不正な引数>` | [process argv contract](02-architecture.md#process-argv-contract) に従い、clap の利用方法エラーとして拒否する |

`usagi session ...` と `usagi mcp` は、daemon が停止していれば起動する。稼働中の daemon が
その repository をまだ開いていない場合は、**repository root で実行したときに限り**その repository を
adopt させてから接続する（[4. IPC#workspace fence](04-ipc.md#workspace-fence)）。したがって新しい repository を
CLI や MCP から使い始めるのに、先に TUI で開いておく必要はない。repository root 以外で実行した場合は、
どの workspace を指しているかが定まらないものとして拒否し、repository root で実行するか `usagi open` で明示的に
開くよう案内する（`usagi open` は repository でない directory も開ける）。adopt 済みになった後は、その配下の
どこで実行しても同じ workspace に解決される。

Welcome 画面は対話的に動く。合成ルートが端末を raw mode + 代替スクリーンにして、TUI 面の
純粋な制御ループ（`presentation::run`）へ注入した端末（`Terminal` ポート）でキー入力を処理する。
実端末の制御（crossterm による raw mode・cursor・mouse・入力 event pump）は合成ルートだけが持ち、
終了時はこれらと代替スクリーンを復元する。描画は TUI 面が返す ANSI/Unicode 幅対応の frame diff を
cursor 移動と変更 span の write に変換し、resize は diff base を無効化して次 frame を全消去・再描画する。
TUI 面は `Terminal` ポートに対して純粋に振る舞う。非対話環境（パイプ・CI など）では対話ループの
代わりに Welcome の 1 フレームを出力して終了する。

Welcome の **Open** を選ぶと workspace 一覧（Open 画面）へ進む。登録済み workspace
（`workspaces.json`）を名前・最終利用の相対時刻で並べ、`↑↓` で選択して Enter で開く。`/` は
名前の部分一致 filter を開始し、Enter で filter 入力を確定する。`u` は Single と Unite を切り替え、
Unite では Space で複数の workspace を選び、Enter で registry 順に開く。`c` は欠損した
ディレクトリを指す registry entry の削除を確認し、`y` でだけ削除する。Welcome 右側の Recent は
最終利用日時が新しい順に最大 3 件を表示し、`1`〜`3` で一覧を経由せず同じ Workspace 画面を開く。
どちらの導線も開いた workspace の最終利用日時を更新する。

Welcome の **New** を選ぶと新規 workspace 作成フォーム（New 画面）へ進む。`↑↓` でフィールドを移り、
モード選択では `←→` で Clone / Existing を切り替え、テキスト欄では文字入力・Backspace・`←→` の
キャレット移動で編集する。Enter で Clone の作成または Existing の登録を実行し、成功するとその
workspace を開いて Home へ遷移する。失敗時は notice を表示し、入力中の draft を保ったまま New に留まる。
入力検証と作成フローの詳細は [TUI の画面と入力](03-tui.md#画面と入力) を正本とする。Esc で Welcome へ戻る。

Welcome の **Config**、または `usagi config` を選ぶと設定画面（Config 画面）へ進む。`Global` の Theme / Modal mode / PR auto-open と、
`Workspace init` の Agent / Issue / Memory を表示し、`↑↓` で項目と Save を選ぶ。Theme と Modal mode は `←→` で編集し、
Modal mode は Overview / Closeup で action を選択する **Action** と command を入力する **Prompt** を切り替える。
Agent はインストール済み CLI に対応する `Claude` / `OpenAI` だけを表示し、新しい Agent pane の既定 profile としてそれぞれ `claude` / `codex` を選ぶ。どちらの CLI もない場合は灰色で無効化する。
Issue と Memory は対応する MCP tool 群を on / off し、どちらも Global の初期値では on である。
変更があるときだけ Save を有効にする。保存成功時は `saved` を表示して Welcome へ戻り、保存失敗時は draft を保って
error を表示する。Theme と Modal mode は user data directory の `settings.json` から全体へ適用する。Agent / Issue / Memory は
同じファイルから新規 workspace の `.usagi/settings.json`（development mode は `.usagi/dev/settings.json`、local mode は
`.usagi/local/settings.json`）へ登録時に一度コピーし、作成済み workspace へ後から反映しない。
Overview の `config` は Home 上の overlay modal として Agent / Issue / Memory だけを表示し、scope 表示を置かない。
settings resolution と entry lifecycle の正本は [TUI の settings scope](03-tui.md#settings-scope-と-workspace-entry)
である。Esc で Welcome へ戻る（`usagi config` から直接開いた場合も Welcome が home）。合成ルートは対話ループの
開始画面を Welcome か Config かで選び、どちらも同じループを回す。

Workspace 画面は、`state.json` から読んだ session 一覧と root 行を左ペイン、選択中 session の
タブを右ペインに表示し、ヘッダーに **Switch** / **Closeup** の現在 mode を示す。起動時は Switch
で、`↑↓`（`j` / `k`）で session と root の選択を循環し、`←→`（`h` / `l`）で Preview / Terminal /
Diff / Notes のタブを循環する。Enter または `t` で選択行の Closeup に入り、session action の
モーダルを workspace とタブの上へ重ねる。Closeup では `↑↓` で action を選び、`←→` で背面の
タブを切り替える。Closeup から Switch へ戻る操作は `Ctrl-O` prefix で行う。live tab に focus がある間は
Esc、Ctrl-C、Ctrl-D、Ctrl-Q を含む leader 以外のキー入力をすべて pane へ渡す。`Ctrl-O Ctrl-A` は
Closeup action モーダルを前面に出す。

Switch で session 行を選択しているとき、`x` は `session remove`、`Shift`+`x`（`X`）は
`session remove -f` を実行する。`-f` は dirty な worktree と未マージの session ブランチの両方を破棄する
（詳細は [3. TUI](03-tui.md#home-と-target)）。root と `+ new session` 行では削除しない。

`:` はどちらの mode からも Overview モーダルを開く。文字入力・Backspace・
`←→` のキャレット移動と `↑↓` の候補選択ができ、Esc で開く前の mode、session、tab へ戻る。
Workspace entry は各 session の daemon PR snapshot を読み、sidebar の右端に PR アイコンと件数を表示する。
Switch の `p`、Closeup の `Ctrl-O Ctrl-P`、または右端の PR 表示のクリックは、同じ snapshot projection から
対象 session の Pull Request モーダルを開き、root では `p` により空一覧を表示する。新しい PR URL の検知時は、別の modal や Director drawer が前面になければ
対象 session の Pull Request モーダルを検知した PR を選択して自動で開く。初回 snapshot の既存 PR は
自動表示しない。`v` は対象の
preview、`d` は diff、`n` は scratchpad の Notes を長文 overlay として開く。`↑↓`（`j` / `k`）で
長文を scroll し、データを提供できない diff や空の Notes は安全な fallback を表示する。いずれも
Home 背景を保ったまま合成し、モーダル表示中はその入力が背面より優先されるため、Overview に入力した
`q` は終了キーにならない。Closeup の `terminal` は空引数または `open` で選択 target の既存 terminal を
完全な identity で再利用し、存在しない場合は daemon に launch を依頼する。`terminal new` は選択 target の
worktree を cwd としてプラットフォーム標準の terminal を別ウィンドウで開き、Closeup を維持する。その他の引数は
安全な feedback で拒否する。terminal stream の IPC 境界は [daemon IPC](04-ipc.md#generic-terminal-request) が正本である。
Overview の `session create <name>`、`session list`、`session overview`、
`session remove <name> [--force]` は daemon IPC へ request を送る。remove は command に明示した
session 名だけに作用し、現在選択中の row や root を暗黙の対象にしない。
Overview の `daemon` は daemon health、process metrics、session 状態別件数、Agent concurrency をまとめた
読み取り専用 modal を開く。daemon の Agent runtime 一覧も scope、状態、短縮 ID とともに表示し、live Agent は対象 tab の
`Ctrl-D` で終了する。表示内容と未報告時の縮退は [TUI の Overview と modal](03-tui.md#overview-と-modal) を正本とする。
左ペインは terminal の wake-up ごとに daemon の session snapshot を再取得するため、MCP など別クライアントが
作成・削除した session も表示へ反映する。session command の実行中は、その完了時の snapshot で同じ同期を行う。
Closeup の `close [-f|--force]` は同じ session checklist を開く。文法、force、keyboard 操作は
[TUI の Overview と modal](03-tui.md#overview-と-modal) が正本である。

`state.json` が未作成なら空の workspace state で開く。一方、既存ファイルを読めない、または解析できない
場合は state を空として扱わず、起動エラーを表示して Workspace 画面を開始しない。

`session remove -s [--force]` は削除対象を複数選ぶ checklist modal を開く。選択 modal の入力、snapshot
reconciliation、Closeup/Switch への復帰は [TUI](03-tui.md#overview-と-modal) が正本である。

Esc は最前面のモーダルを閉じる。Switch / Closeup の背景では mode や画面遷移を起こさない。Switch からの
Open・Welcome への遷移と直接起動した `usagi open` の終了は、明示的な終了操作で行う。
`q` は基底の Switch / Closeup で確認後に TUI を閉じ、daemon の実行は継続する。Ctrl-Q も確認 modal を開き、
確認するとこの client を detach する（daemon の terminal や session は停止しない）。Switch の Ctrl-C は TUI を終了しない。
terminal tab は daemon が所有する terminal だけを表示する。選択中の live terminal は PTY 出力を右ペインへ
描画し、focus 中の通常キーをその PTY へ送るため、`ls` などを対話的に実行できる（`q` は shell へ渡り、終了には
ならない）。tab 巡回や Switch への復帰は `Ctrl-O` prefix だけが所有し、その他の入力は live terminal へ渡す。
出力表示・入力送信の詳細は [TUI の live terminal](03-tui.md#closeup-pane) が正本である。client の detach は
tab の subscription を外すだけで process を停止しない。

`usagi open [path]` も同じ Workspace 画面を直接起動する。相対 path と省略時のカレントディレクトリは
実在する絶対 path へ解決し、未登録ならディレクトリ名を workspace 名として登録する。同名が既に別
path に使われている場合は `-2`、`-3` と suffix を付ける。JSON の path string で表せない非 UTF-8
path も、filesystem が実在する絶対ディレクトリとして解決できる場合は、
その起動中だけの workspace として開き registry には保存しない。実在性を検証できない path や
通常 file は開かない。

非対話環境（パイプ・CI など）では、選ばれた Welcome / Config / Workspace の 1 フレームを出力して
終了する。Doctor は Git と任意の Agent CLI の導入状況、設定ストレージの読み込み、daemon の起動・接続を
診断し、項目ごとの結果と全体の成否を出力して終了する。
