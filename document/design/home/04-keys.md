# 5.4 キー操作

> [ホーム画面トップ](README.md) ｜ ← 前へ [5.3 サイドバー](03-sidebar.md) ｜ 次へ → [5.5 通知・タスク・モーダル](05-overlays.md)

ホーム画面のキー操作は、トップレベル mode（Switch / Closeup）と modal（Overview / Focus）で整理します。

## 目次

- [Switch](#switch)
- [Closeup](#closeup)
- [Overview モーダル](#overview-モーダル)
- [Focus モーダル](#focus-モーダル)
- [ライブ端末（Closeup 内部状態）](#ライブ端末closeup-内部状態)

## Switch

| キー | 動作 |
|---|---|
| `↑↓` / `k j` | セッション行を移動 |
| `←→` / `h l` / `Ctrl-P` / `Ctrl-N` | 選択中セッションのタブを切り替える |
| `Enter` | ライブなら Closeup のライブ端末へ、非ライブなら Focus モーダルへ |
| `t` | 選択中セッションの Focus モーダルを開く |
| `Ctrl-O a` | 選択中セッションの Focus モーダルを開く |
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
| `Ctrl-O a` | Focus モーダルを開く（ペイン preview 上では preview の上に浮かぶ） |
| `Ctrl-O o` / `Ctrl-O Ctrl-O` | Switch へ戻る |
| `Ctrl-O g` | agent を起動 |
| `Ctrl-O e` | メモ編集 |
| `Ctrl-O s` | サイドバー開閉 |
| `Ctrl-O q` | 終了確認モーダル |
| `:` | Overview モーダルを開く |
| `Esc` | Focus モーダルや preview を一段戻し、最後は Switch へ戻る |

`alt` キー方式では、ライブ端末の予約操作は `Alt-o` / `Alt-a` / `Alt-g` のような単打になります。Closeup の非ライブ UI では `Ctrl-O` は Switch へ戻る直キーとして扱います。

## Overview モーダル

`:` で開く Workspace スコープのモーダルです。`Tab` で補完、`↑↓` で履歴、`Enter` で実行、`Esc` で閉じます。

## Focus モーダル

`Ctrl-O a` / `t` / 非ライブセッションの確定で開く Session スコープのモーダルです。Menu では `↑↓` で選択、`Enter` で実行、`/` で filter、`Esc` で戻ります。Prompt ではセッションスコープのコマンドラインとして編集・補完できます。

## ライブ端末（Closeup 内部状態）

ライブ端末中は通常のキーをシェル / Agent へ流します。予約キーだけを usagi が処理します。

| キー（prefix 方式） | 動作 |
|---|---|
| `Ctrl-O o` | Switch へ戻る |
| `Ctrl-O a` | Focus モーダルを開く |
| `Ctrl-O n` / `Ctrl-O p` / `Ctrl-O →` / `Ctrl-O ←` | タブ切替 |
| `Ctrl-O g` | agent タブを追加 / 既存 agent タブへ移動 |
| `Ctrl-O e` | メモ編集（閉じると同じペインへ戻る） |
| `Ctrl-O x` | アクティブタブを閉じる |
| `Ctrl-O q` | 終了確認モーダル |
| `Ctrl-^` | 直前のセッションへ |

