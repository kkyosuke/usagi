---
number: 127
title: refactor(tui): Nerd Font グリフ語彙を 1 モジュールに集約し FA4 注記重複・SEP 重複を解消する
status: done
priority: medium
labels: [refactor, tui, review]
dependson: []
related: []
created_at: 2026-07-04T23:16:32.586038+00:00
updated_at: 2026-07-04T23:16:32.586038+00:00
---

## 背景（なぜ問題か）

Nerd Font のアイコン定数が層をまたいで散在している — `home/ui/mod.rs`（`NEW_ICON`/`DIRTY_ICON`/`LOCAL_ICON`/`PUSHED_ICON`/`SYNCED_ICON`/`NOTE_ICON`）、`home/ui/panes.rs`（`AGENT_ICON`/`SELECTED_SESSION_GLYPH`/`CPU_ICON`/`MEM_ICON`/`PR_ICON`）、`home/ui/chrome.rs`（`WAITING_ICON`）、`open/ui.rs`（独自の絵文字系）。さらに「FA5 ではなく Font Awesome 4 レンジを使う（古い Nerd Font で `?` にならないため）」という同じ設計根拠コメントが `AGENT_ICON`・`MEM_ICON`・`PR_ICON` の 3 箇所に書き分けられている。グリフ選定ポリシーの SSoT が無く、新規アイコン追加時に FA4 レンジ規約を見落としやすい。

加えてペイン区切り文字 `" │ "` が `ui/mod.rs::SEP` ・ `panes.rs::HEADER_TAB_DIVIDER` ・ `open/ui.rs` の 3 箇所でリテラル重複している（`HEADER_TAB_DIVIDER` のコメントは「SEP を再利用」と言いながら実際は再定義）。

## 対象箇所

上記各ファイルのアイコン/グリフ定数、`SEP` / `HEADER_TAB_DIVIDER` の `" │ "`。

## やること

- `presentation/tui/glyphs.rs`（または `theme` 隣接）に Nerd Font グリフ定数を集約し、FA4 レンジ採用の根拠をモジュールドックに 1 度だけ記す。各 `const` はそこを参照する。
- `HEADER_TAB_DIVIDER` は `SEP` を参照するよう変更する。

## 受け入れ条件

- アイコン定数の定義が 1 モジュールに集約され、FA4 根拠コメントが 1 箇所になる。`" │ "` リテラルが 1 定義になる。
- 既存テストが緑、カバレッジ 100% 維持。

## 補足

用語ポリシー整理 #95（todo）とは対象が異なる（こちらはアイコン=コード定数の SSoT 化）。
