# 5. ホーム画面（Home）

> [画面設計トップ](../README.md) ｜ ← 前へ [4. 設定画面（Config）](../04-config.md)

プロジェクト選択画面でワークスペースを選ぶと遷移する、ワークスペース操作のメイン画面。
worktree 一覧（左ペイン）と右ペイン、下部の入力・フッターで構成し、トップレベルの操作モードは
**Switch** と **Closeup** の 2 つだけです。

- **Switch**: セッション群を操作する。左ペインで選ぶ・作る・並べ替える・タブを切り替える。
- **Closeup**: 選択中セッションの中を操作する。Focus モーダル（Menu / Prompt）またはライブな埋め込み端末を扱う。

`Overview` と `Focus` はモード名ではなくモーダル名です。`:` は **Overview モーダル**（Workspace スコープのコマンド面）を開き、`Ctrl-O a` は **Focus モーダル**（Session スコープのアクション面）を開きます。ライブ端末は Closeup の内部状態であり、第 3 のモードではありません。

> TUI 内コマンドの一覧・引数は [../03-commands/02-tui.md](../../03-commands/02-tui.md) に集約しています。
> 本書は画面レイアウトとモード・キー操作に絞ります。

## 目次（ホーム画面の詳細）

| # | ドキュメント | 内容 |
|---|---|---|
| 5.1 | [01-modes.md](01-modes.md) | Switch / Closeup と Overview / Focus モーダル |
| 5.2 | [02-layout.md](02-layout.md) | レイアウトと各モードの表示 |
| 5.3 | [03-sidebar.md](03-sidebar.md) | サイドバー（左ペインの行表示） |
| 5.4 | [04-keys.md](04-keys.md) | キー操作 |
| 5.5 | [05-overlays.md](05-overlays.md) | 通知・タスク・モーダル |
