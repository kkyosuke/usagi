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
（[v1/document/proposals/02-daemon.md](../v1/document/proposals/02-daemon.md)）を
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
| `usagi config` | Config 画面を対話的に表示する（`Esc` で Welcome へ、`Ctrl-C` で終了） |
| `usagi doctor` | Doctor TUI を起動画面に選ぶ |
| `usagi version` / `usagi --version` | 配布 version を表示する |
| `usagi daemon` | daemon 面の ready 行（`usagi v<version> daemon ready`）を表示する |
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

Welcome の **Config**、または `usagi config` を選ぶと設定画面（Config 画面）へ進む。設定項目はまだ
無く、Esc で Welcome へ戻る（`usagi config` から直接開いた場合も Welcome が home）。合成ルートは
対話ループの開始画面を Welcome か Config かで選び、どちらも同じループを回す。

Workspace 画面は、`state.json` から読んだ session 一覧と root 行を左ペイン、選択中 session の
タブを右ペインに表示し、ヘッダーに **Switch** / **Closeup** の現在 mode を示す。起動時は Switch
で、`↑↓`（`j` / `k`）で session と root の選択を循環し、`←→`（`h` / `l`）で Preview / Terminal /
Diff / Notes のタブを循環する。Enter または `t` で選択行の Closeup に入り、session action の
モーダルを workspace とタブの上へ重ねる。Closeup では `↑↓` で action を選び、`←→` で背面の
タブを切り替え、Esc で Switch へ戻る。

`:` はどちらの mode からも Workspace scope の Overview モーダルを開く。文字入力・Backspace・
`←→` のキャレット移動と `↑↓` の候補選択ができ、Esc で開く前の mode、session、tab へ戻る。
`p` は選択中 session の Pull Request モーダルを開き、root では空一覧を表示する。モーダル表示中は
その入力が背面より優先されるため、Overview に入力した `q` は終了キーにならない。Closeup action、
Overview command、Pull Request の実行はまだ接続せず、今回は表示と選択だけを行う。

Esc は最前面から `モーダル → Closeup → Switch → 呼び出し元` の順に戻る。Switch からは Open
経由なら Open、Recent 経由なら Welcome へ戻り、`usagi open` で直接開いた場合は終了する。
`q` は基底の Switch / Closeup で TUI を終了し、最前面モーダルではそのモーダルが受け取る。
Ctrl-C は表示状態にかかわらず TUI を終了する。タブ本文はプレースホルダで、daemon が所有する
端末への attach はまだ行わない。

`usagi open [path]` も同じ Workspace 画面を直接起動する。相対 path と省略時のカレントディレクトリは
実在する絶対 path へ解決し、未登録ならディレクトリ名を workspace 名として登録する。同名が既に別
path に使われている場合は `-2`、`-3` と suffix を付ける。JSON の path string で表せない非 UTF-8
path も、filesystem が実在する絶対ディレクトリとして解決できる場合は、
その起動中だけの workspace として開き registry には保存しない。実在性を検証できない path や
通常 file は開かない。

非対話環境（パイプ・CI など）では、選ばれた Welcome / Config / Workspace の 1 フレームを出力して
終了する。Doctor は対話ループを持たず、選択された画面名を `BannerScreenRunner` が表示する。
