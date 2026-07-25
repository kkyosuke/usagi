# 9. 環境変数設定

> [ドキュメント目次](README.md) ｜ ← 前へ [8. coverage exclusion inventory](08-coverage.md)

usagi が起動する Agent / terminal の子プロセスへ注入する環境変数の設定。**本書が env 設定の正本**である
（保存場所・スコープの合成・secret の解決・注入のタイミング）。編集 UI の操作は
[3. TUI#env editor](03-tui.md#env-editor)、クレート責務は [2. アーキテクチャ](02-architecture.md) を参照する。

## 目次

- [スコープと保存場所](#スコープと保存場所)
- [binding の書式と検証](#binding-の書式と検証)
- [実効環境の合成](#実効環境の合成)
- [secret の解決](#secret-の解決)
- [注入のタイミングと優先順位](#注入のタイミングと優先順位)

## スコープと保存場所

env は 2 層で持つ。global は全 workspace が継承し、workspace はそこへ追加・上書きする。

| スコープ | 保存先 | JSON キー | 効く範囲 |
|---|---|---|---|
| global | `<データディレクトリ>/settings.json` | `env` | すべての workspace |
| workspace | `<workspace>/.usagi/settings.json` | `env` | その workspace（root / session の全 pane） |

- データディレクトリの解決は `$USAGI_HOME` → `~/.usagi` で、runtime mode（`dev` / `local`）の
  サブディレクトリを含む。
- どちらも `name → value` の map で、名前順に並ぶ。値は**平文**か**secret reference**のいずれか
  （[binding の書式と検証](#binding-の書式と検証)）。**解決済みの secret は保存しない**。
- workspace 設定は Agent / Issue / Memory の設定と同じファイルを共有する。Config 画面の保存は
  `env` を保持したまま Agent / Issue / Memory の値だけを書き換える（実効値の merge 結果を
  workspace ファイルへ写し取らない）。

## binding の書式と検証

editor は 1 行 1 binding の `NAME=value` を受け取り、保存時に次の検証を通ったものだけを永続化する。
保存されたものが、そのまま注入される候補になる。

| 項目 | 規則 |
|---|---|
| 名前 | 先頭は英字または `_`、以降は英数字または `_`（移植可能な識別子）。それ以外は破棄 |
| 値 | 前後の空白を除去する。空値・NUL を含む値は破棄（editor では空値は**削除**を意味する） |
| secret 参照 | `op://` で始まり、続くパスが空でない値。それ以外は平文として扱う |
| 重複 | 同名は後の行が勝ち、map は名前順に正規化される |

## 実効環境の合成

実効 binding は global に workspace を重ねた結果で、同名は workspace が勝つ。

```
global settings.json   env: { GH_TOKEN: op://…, RUST_LOG: info }
                              │              │
workspace settings.json  env: {              RUST_LOG: debug, PROJECT: usagi }
                              ▼              ▼
実効 binding            { GH_TOKEN: op://…, RUST_LOG: debug, PROJECT: usagi }
```

workspace の editor は、この継承関係を隠さない。編集中のスコープ自身の binding とは別に
**global の binding を read-only で併記**し、workspace 側が同名を持つものは上書き済みとして示す
（[3. TUI#env editor](03-tui.md#env-editor)）。

## secret の解決

平文の値は解決を要さずそのまま注入する。`op://` の値だけを 1Password CLI（`op read --no-newline`）で
解決する。

- 解決は **binding ごとに並列**（1 参照 = 1 subprocess）で行うため、待ち時間は個々の合計ではなく
  最も遅い 1 件ぶんになる。1 件あたり 30 秒の deadline を持つ。
- `op` の認証は CLI 側の通常の仕組みに従う（`op signin` セッション、または daemon 自身の環境の
  `OP_SERVICE_ACCOUNT_TOKEN`）。
- **解決に失敗した binding は注入せず、その変数だけを落として error ログに記録する**（変数名と参照は
  記録し、解決値は記録しない）。vault がロックされていても pane は開く。
- 解決結果は workspace ごとに**設定内容をキーにキャッシュ**する。設定が変わらなければ次の pane 起動で
  `op read` を再実行せず、設定を編集すればキャッシュは無効になる。

## 注入のタイミングと優先順位

PTY を所有するのは daemon なので、**daemon が起動時に自分で 2 つの settings ファイルを読み**、解決して
子プロセス環境へ入れる。環境変数の値と secret は IPC の wire field ではない（[4. IPC](04-ipc.md)）。

| 面 | 合成順（後が勝つ） |
|---|---|
| Agent pane | 継承した端末特性 → **設定 env** → adapter の provision → daemon 発行の ephemeral 値 |
| terminal pane | 継承した端末特性（`TERM` / `PATH` など） → **設定 env** |

- 設定 env は端末特性を上書きできるが、daemon が子を daemon 自身へ結び付けるための値（MCP 配線・
  credential）を置き換えることはできない。
- durable な launch snapshot に載るのは**変数名の allowlist だけ**で、値・secret は載らない。
- 反映は**新しく開く pane から**。既に動いている pane は起動時の環境を保ち続ける。
