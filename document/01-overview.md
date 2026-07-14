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

- 配布 version はルート `Cargo.toml` が v1 の version を引き継ぎ、v2 として最初に
  リリースするときに bump する（[6. 開発規約#リリース](06-conventions.md#リリース)）。
- v1 は `v1/` 配下で従来どおり単体ビルドできる。

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
| `usagi update` | GitHub Releases の最新バイナリを download して `~/.usagi/bin/` へ導入する。反映には再起動が必要 |
| `usagi version` / `usagi --version` | 配布 version を表示する |
| `usagi daemon start` | daemon をバックグラウンドで起動し、登録された pid を表示する。すでに稼働中ならその pid を表示する |
| `usagi daemon stop` | 稼働中の daemon を終了する。stale な lifecycle record は回収する |
| `usagi daemon status` | daemon が稼働中か、stale record が回収可能かを表示する |
| `usagi daemon restart` | 稼働中 daemon を停止してから新しい daemon を起動する |
| `usagi daemon` | daemon を前景で serve する（通常は `start` が起動する内部経路） |
| `usagi mcp` | 入口面（MCP）の ready 行（`usagi v<version> mcp ready`）を表示する |
| `usagi <不正な引数>` | clap の利用方法エラーを stderr に表示し、非 0 で終了する |

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
キャレット移動で編集する。Esc で Welcome へ戻る。フォームの確定（実際の作成）は作成処理が入るまで
留まる。

Welcome の **Config**、または `usagi config` を選ぶと設定画面（Config 画面）へ進む。Tab で global /
workspace scope を切り替え、`↑↓` で Theme / Modal mode / Agent model / Save を選ぶ。Theme と Modal mode は `←→` で編集し、
Modal mode は Overview / Closeup で action を選択する **Action** と command を入力する **Prompt** を切り替える。
Agent model は `Claude` / `OpenAI` を切り替え、新しい Agent pane の既定 profile としてそれぞれ `claude` / `codex` を選ぶ。
scope ごとに独立した draft と dirty state を持ち、変更があるときだけ Save を有効にする。保存成功時は `saved` を表示して
Welcome へ戻り、保存失敗時は draft を保って error を表示する。Modal mode と Agent model は global 設定として `settings.json` に保存され、次に開く Workspace の Overview / Closeup と Agent pane に適用される。Esc で Welcome へ戻る（`usagi config` から直接開いた場合も Welcome が home）。合成ルートは対話ループの
開始画面を Welcome か Config かで選び、どちらも同じループを回す。

Workspace 画面は、`state.json` から読んだ session 一覧と root 行を左ペイン、選択中 session の
タブを右ペインに表示し、ヘッダーに **Switch** / **Closeup** の現在 mode を示す。起動時は Switch
で、`↑↓`（`j` / `k`）で session と root の選択を循環し、`←→`（`h` / `l`）で Preview / Terminal /
Diff / Notes のタブを循環する。Enter または `t` で選択行の Closeup に入り、session action の
モーダルを workspace とタブの上へ重ねる。Closeup では `↑↓` で action を選び、`←→` で背面の
タブを切り替える。Closeup から Switch へ戻る操作は `Ctrl-O` で行い、Esc は mode を変えない。

`:` はどちらの mode からも Workspace scope の Overview モーダルを開く。文字入力・Backspace・
`←→` のキャレット移動と `↑↓` の候補選択ができ、Esc で開く前の mode、session、tab へ戻る。
`p` は選択中 session の Pull Request モーダルを開き、root では空一覧を表示する。`v` は対象の
preview、`d` は diff、`n` は scratchpad の Notes を長文 overlay として開く。`↑↓`（`j` / `k`）で
長文を scroll し、データを提供できない diff や空の Notes は安全な fallback を表示する。いずれも
Home 背景を保ったまま合成し、モーダル表示中はその入力が背面より優先されるため、Overview に入力した
`q` は終了キーにならない。Closeup の `terminal` は空引数または `open` で選択 target の既存 terminal を
完全な identity で再利用し、存在しない場合は daemon に launch を依頼する。`terminal new` は常に daemon
launch を依頼する。その他の引数は安全な feedback で拒否し、local PTY や name/path lookup には fallback
しない。terminal stream の IPC 境界は [daemon IPC](04-ipc.md#generic-terminal-request) が正本である。
Overview の `session create <name>`、`session list`、`session overview`、
`session remove <name> [--force]` は daemon IPC へ request を送る。remove は command に明示した
session 名だけに作用し、現在選択中の row や root を暗黙の対象にしない。
Closeup の `close [-f|--force]` は同じ session checklist を開く。文法、force、keyboard 操作は
[TUI の Overview と modal](03-tui.md#overview-と-modal) が正本である。

`session remove -s [--force]` は削除対象を複数選ぶ checklist modal を開く。選択 modal の入力、snapshot
reconciliation、Closeup/Switch への復帰は [TUI](03-tui.md#overview-と-modal) が正本である。

Esc は最前面のモーダルを閉じる。Switch / Closeup の背景では mode や画面遷移を起こさない。Switch からの
Open・Welcome への遷移と直接起動した `usagi open` の終了は、明示的な終了操作で行う。
`q` は基底の Switch / Closeup で確認後に TUI を閉じ、daemon の実行は継続する。Ctrl-Q は確認後に
workspace の live session を終了してから TUI を閉じる。Ctrl-C は表示状態にかかわらず TUI を終了する。
terminal tab は daemon が所有する terminal だけを表示し、client の detach は tab の subscription を外すだけで
process を停止しない。

`usagi open [path]` も同じ Workspace 画面を直接起動する。相対 path と省略時のカレントディレクトリは
実在する絶対 path へ解決し、未登録ならディレクトリ名を workspace 名として登録する。同名が既に別
path に使われている場合は `-2`、`-3` と suffix を付ける。JSON の path string で表せない非 UTF-8
path も、filesystem が実在する絶対ディレクトリとして解決できる場合は、
その起動中だけの workspace として開き registry には保存しない。実在性を検証できない path や
通常 file は開かない。

非対話環境（パイプ・CI など）では、選ばれた Welcome / Config / Workspace の 1 フレームを出力して
終了する。Doctor は対話ループを持たず、選択された画面名を `BannerScreenRunner` が表示する。
