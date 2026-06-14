# 3.2 TUI 内コマンド

> [コマンドリファレンス目次](README.md) ｜ ← 前へ [CLI コマンド](01-cli.md)

`usagi hop` のホーム画面でコマンドモード（`:` または `i`）に入って実行するコマンドの一覧です。
`Tab` で補完、`↑↓` で履歴を遡れます。状態記号の凡例は [README.md](README.md#凡例) を、
画面側の挙動は [design/05-home.md](../design/05-home.md) を参照してください。

| コマンド | 説明 | issue | 状態 |
|---|---|---|---|
| `man` / `help` | コマンド一覧、または `man <command>` で個別の説明を表示 | [008](../../issues/008-man.md) | ✅ |
| `history` | 入力したコマンドの履歴を番号付きで表示 | [007](../../issues/007-history.md) | ✅ |
| `clear` | 右ペインの出力ログを消去 | — | ✅ |
| `quit` / `exit` | アプリを終了 | — | ✅ |
| `session` | `session new <name>` でセッション（`.usagi/worktree/<name>/` 配下に再帰的に worktree を構築）を作成（`session new` と名前省略時は名前入力モーダル）。`session list` で一覧、`session switch <name>` でアクティブセッション切り替え（引数なしで一覧、worktree 一覧の Enter でも切り替え）、`session remove <name> [--force]` で削除（未コミット変更があれば警告し `--force` で破棄） | [003](../../issues/003-session.md) / [004](../../issues/004-space.md) | ✅ 実装済み |
| `ai` | 選択中の Agent CLI を起動し、現在の worktree をコンテキストに AI へ指示・対話する | [005](../../issues/005-ai.md) | 🚧 |
| `terminal` | 選択中の worktree（未選択時はワークスペースルート）を作業ディレクトリに対話型シェルを**右ペインに埋め込んで**起動する。左ペインの worktree 一覧は表示したまま。`Ctrl-O` でデタッチしてコマンドモードへ戻る（**シェルはバックグラウンドで動作継続**。再実行で再アタッチ） | [006](../../issues/006-terminal.md) | ✅ 実装済み |
| `agent` | `terminal` と同じ埋め込みシェルを開き、設定中の Agent CLI（既定は `claude`、ローカル設定で上書き可）を起動コマンドとして自動入力する。実質 `terminal` → `claude` のショートカット。`Ctrl-O` でデタッチしても Agent はバックグラウンドで動作継続し、**入力待ちになると左ペインに `◆` マーカー＋デスクトップ通知**で知らせる | [026](../../issues/026-agent.md) / [027](../../issues/027-agent-wait-notify.md) | ✅ 実装済み |
| `doctor` | 依存関係チェック（TUI 版） | [019](../../issues/019-doctor-fix.md) | 🚧 |
| `diff` | TUI Diff ビューア（セッションの差分閲覧） | [012](../../issues/012-diff.md) | 🚧 |

> 🚧 のうち `ai` / `doctor` はホーム画面で名前としては認識され、本体が未実装のため「coming soon」を
> 表示します（プレースホルダーとして登録済み）。`diff` はまだコマンドとして登録されておらず、入力すると
> `unknown command` になります。`session` / `ai` などが司る worktree オーケストレーションの全体像は
> [4. オーケストレーション](../04-orchestration.md) を参照してください。
>
> `terminal` は左ペインの worktree 一覧で選択中の worktree を作業ディレクトリに、**右ペインへ埋め込んだライブシェル**を開きます（疑似ターミナル: portable-pty + vt100）。左ペインの一覧は表示したままなので、シェルを操作しながら worktree を見渡せます。`Ctrl-O` でデタッチするとコマンドモードへ戻りますが、**シェルはバックグラウンドで動き続け**、同じ worktree で再度 `terminal` を実行すると動作中のセッションへ再アタッチします（シェル側で `exit` するとそのセッションは破棄されます）。`session new` で作ったセッションの worktree を選んで `terminal` を実行すれば、そこで `claude` などの AI エージェントを起動して開発できます。
>
> `agent` はその「`terminal` を開いて Agent CLI を起動する」一連の操作をまとめたショートカットです。`terminal` と同じ埋め込みシェルを起動し、設定の Agent CLI（既定は `claude`。Config 画面やローカル設定で `gemini` などに変更可能）をそのまま入力します。`Ctrl-O` でデタッチしても Agent はバックグラウンドで動作を続け、処理を終えて入力待ちになる（ターミナルベルを鳴らす）と、左ペインの該当 worktree に `◆`（黄色）マーカーが付き、デスクトップ通知（`🐰 <ブランチ名> が入力待ちです`、設定 `notifications_enabled` が ON のとき）で知らせます。複数セッションを並行で走らせ、入力が必要になったものだけに対応する、といった使い方ができます。検知はベルに依存するため `claude` 側でターミナルベル通知を有効にしてください。
