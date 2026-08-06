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
- settings file は Config の設定と共有する。Config 画面の保存は scope lock 内で最新 file を読み直し、global / workspace
  どちらの `env` も保持する。Workspace Config は Agent / Issue / Memory の値だけを書き換え、実効値の merge 結果を
  workspace ファイルへ写し取らない（詳しい field ownership は [TUI](03-tui.md#settings-scope-と-workspace-entry)を正本とする）。

## binding の書式と検証

editor は 1 行 1 binding の `NAME=value` を受け取り、保存時に次の検証を通ったものだけを永続化する。
保存されたものが、そのまま注入される候補になる。

| 項目 | 規則 |
|---|---|
| 名前 | 先頭は英字または `_`、以降は英数字または `_`（移植可能な識別子）。それ以外は破棄 |
| 値 | 前後の空白を除去する。空値・NUL を含む値は破棄（editor では空値は**削除**を意味する） |
| secret 参照 | `op://` で始まり、続くパスが空でない値。それ以外は平文として扱う |
| 重複 | 同名は後の行が勝ち、map は名前順に正規化される |

binding と secret reference の resource 上限は domain の env policy が正本であり、global / workspace の各保存文書と
合成後の launch admission が同じ検証を使う。

| 対象 | 上限 | 超過時 |
|---|---:|---|
| 1 scope または合成後の binding | 128 | 保存・load または launch admission を拒否 |
| 1 scope または合成後の secret reference | 32 | 保存・load または launch admission を拒否 |
| 1 launch で同時実行する `op read` | 4 | 残りを bounded queue で待機 |

上限超過を launch admission で検出した場合は secret resolver と PTY child を一つも spawn せず、安全な validation / provision
error を返す。global と workspace がそれぞれ保存上限内でも、合成後に上限を超える組み合わせは同じように拒否する。

## 実効環境の合成

実効 binding は global に workspace を重ねた結果で、同名は workspace が勝つ。

```
global settings.json   env: { GH_TOKEN: op://…, RUST_LOG: info }
                              │              │
workspace settings.json  env: {              RUST_LOG: debug, PROJECT: usagi }
                              ▼              ▼
実効 binding            { GH_TOKEN: op://…, RUST_LOG: debug, PROJECT: usagi }
```

Overview / Closeup の workspace editor は、この継承関係を隠さない。編集中のスコープ自身の binding とは別に
**global の binding を read-only で併記**し、workspace 側が同名を持つものは上書き済みとして示す。
Workspace Config は scope の誤編集を避けるため workspace binding だけを表示する
（[3. TUI#env editor](03-tui.md#env-editor)）。

global binding は Welcome の Config にある `Env  [ N variables ]` から編集できる。
Overview の `env global` も同じ global settings を編集する。workspace binding は Workspace Config の同じ行、Overview、
Closeup の `env` から編集する。
Config は editor を開く直前に対象 scope の最新値を複数行 textarea へ読み、対象 scope の `env` だけを全置換保存する。
Workspace Config は global binding を表示・変更しない。

## secret の解決

平文の値は解決を要さずそのまま注入する。`op://` の値だけを 1Password CLI（`op read --no-newline`）で
解決する。

- 解決は最大 4 worker の bounded queue で行う（1 参照 = 1 subprocess）。1 件あたり 30 秒の deadline を持つ。
  binding の結果は完了順でなく名前順へ戻して merge する。
- deadline を超えた `op` は、その child handle の owner が exact child だけへ graceful terminate を送り、2 秒の bounded wait
  後も残る場合は kill する。その後は wait/reap と stdout / stderr reader の join を終えてから failure を返す。任意 PID や
  owner が証明できない process は signal 対象にしない。
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
