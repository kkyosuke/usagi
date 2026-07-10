# 5.4 キー操作

> [ホーム画面トップ](README.md) ｜ ← 前へ [5.3 サイドバー](03-sidebar.md) ｜ 次へ → [5.5 通知・タスク・モーダル](05-overlays.md)

ホーム画面のキー操作は、トップレベル mode（Switch / Closeup）と modal（Overview / Closeup）で整理します。

## 目次

- [Switch](#switch)
- [Closeup](#closeup)
- [Overview モーダル](#overview-モーダル)
- [Closeup モーダル](#closeup-モーダル)
- [ライブ端末（Closeup 内部状態）](#ライブ端末closeup-内部状態)

## Switch

| キー | 動作 |
|---|---|
| `↑↓` / `k j` | セッション行を移動 |
| `g` / `G` | 一覧の先頭 / 末尾の行へジャンプ |
| `←→` / `h l` / `Ctrl-P` / `Ctrl-N` | 選択中セッションのタブを切り替える |
| `Enter` | ライブなら Closeup のライブ端末へ、非ライブなら Closeup モーダルへ |
| `t` | 選択中セッションの Closeup モーダルを開く |
| `Ctrl-O a` | 選択中セッションの Closeup モーダルを開く |
| `:` | Overview モーダルを開く |
| `c` / `r` / `n` | 新規セッション作成 / 表示名変更 / メモ編集 |
| `x` | 選択中セッションのアクティブタブを閉じる |
| `Esc` | 無効（Switch が基底 mode） |
| `Ctrl-C` / `Ctrl-Q` | 終了確認モーダル |

## Closeup

| キー | 動作 |
|---|---|
| `Ctrl-N` / `Ctrl-P` | セッション内のタブを巡回 |
| `Enter` | ペイン preview 上ならそのペインへ再アタッチ |
| `Ctrl-O a` | Closeup モーダルを開く（ペイン preview 上では preview の上に浮かぶ） |
| `Ctrl-O o` / `Ctrl-O Ctrl-O` | Switch へ戻る |
| `Ctrl-O g` | agent を起動 |
| `Ctrl-O e` | メモ編集 |
| `Ctrl-O s` | サイドバー開閉 |
| `Ctrl-O q` | 終了確認モーダル |
| `:` | Overview モーダルを開く |
| `Esc` | Closeup モーダルや preview を一段戻し、最後は Switch へ戻る |

`alt` キー方式では、ライブ端末の予約操作は `Alt-o` / `Alt-a` / `Alt-g` のような単打になります。Closeup の非ライブ UI では `Ctrl-O` は Switch へ戻る直キーとして扱います。

## Overview モーダル

`:` で開く Workspace スコープのモーダルです。`Tab` で補完、`↑↓` で履歴、`Enter` で実行、`Esc` で閉じます。

## Closeup モーダル

`Ctrl-O a` / `t` / 非ライブセッションの確定で開く Session スコープのモーダルです。Menu では `↑↓` で選択、`Enter` で実行、`/` で filter、`Esc` で戻ります。Prompt ではセッションスコープのコマンドラインとして編集・補完できます。

## ライブ端末（Closeup 内部状態）

ライブ端末中は通常のキーをシェル / Agent へ流します。予約キーだけを usagi が処理します。

| キー（prefix 方式） | 動作 |
|---|---|
| `Ctrl-O o` | Switch へ戻る |
| `Ctrl-O a` | Closeup モーダルを開く |
| `Ctrl-O n` / `Ctrl-O p` / `Ctrl-O →` / `Ctrl-O ←` | タブ切替 |
| `Ctrl-O g` | agent タブを追加 / 既存 agent タブへ移動 |
| `Ctrl-O e` | メモ編集（閉じると同じペインへ戻る） |
| `Ctrl-O x` | アクティブタブを閉じる |
| `Ctrl-O q` | 終了確認モーダル |
| `Ctrl-^` | 直前のセッションへ |


## ペインのスクロールとマウスホイール

管理画面（Switch / Closeup と Overview / Closeup モーダル）では TUI 自体（一覧やレイアウト）はスクロールしません。ただし**スクロールできる面が開いているとき**——右ペインの diff / Markdown プレビュー、テキストモーダル——は、マウスホイールでその面をスクロールできます（キーの `↑↓` / `PageUp` / `PageDown` と同じ対象）。それ以外（セッション一覧など）でのホイールは、誤って動かさないため読み捨てます。

スクロールできるのは **Closeup のライブ端末**だけです。ライブ端末では、ホイール（対応端末のみ）と `Shift`+`PageUp` / `Shift`+`PageDown`、`Shift`+`↑↓` でシェルのスクロールバックをさかのぼれます。キー入力すると最新画面へ戻ります。全画面プログラムがマウスレポートを有効にしている場合は、ホイールをそのプログラムへ転送します。

## 使用中 Agent の表示（入力待ちの検知と通知）

サイドバーの Agent アイコンは、埋め込み Agent のライフサイクルから状態を表示します。

| 状態 | 表示 | 意味 |
|---|---|---|
| ready | `🤖 ☾` | Agent は起動済みで、まだターンを実行していない |
| running | `🤖 ▶` | Agent がターンを実行中 |
| waiting | `🤖 ◆` | Agent が入力・許可待ち |
| done | `🤖 ✓` | ターン完了またはプロセス終了 |

優先順は `done > waiting > running > ready` です。入力待ち通知は、アタッチ中の自分自身にも出します。完了通知はアタッチ中の自分自身では抑制し、他セッションの完了・入力待ちを見落とさないようにします。
