---
number: 45
title: refactor(tui): HomeState の状態・入力・モーダルの保持を整理する
status: done
priority: medium
labels: [refactor, tui]
dependson: []
related: []
created_at: 2026-06-18T22:40:51.082550+00:00
updated_at: 2026-06-18T22:40:51.082550+00:00
---

## 背景

`HomeState` が責務過多で API 面積が肥大化している。同型の状態・メソッドが複数セット並んでおり、フィールド追加のたびに増殖する。

### 1. `MonitorSnapshot` をばらして二重保持（中）
`HomeState` は `running`/`waiting`/`live`/`done` の 4 つの `HashSet<PathBuf>` を持ち、`set_*`×4・`is_*`×4・`*_paths`×4 の計 12 メソッドを公開している。元の `MonitorSnapshot` が既に 4 セットをまとめた比較可能な型なのに、`event/mod.rs` で毎フレーム 4 回ムーブ格納、`terminal_pane.rs` でも 4 回 clone して展開し直している。`live` セットも `running`/`waiting`/`done` から派生した値で、独立して持つ理由がない。

### 2. インライン入力の編集メソッドが 4 セット重複（中）
Overview / focus prompt / create / rename の各入力に対して `push_char`/`backspace`/`delete_forward`/`cursor_*`/`complete` が別名でほぼ同じものが並ぶ。create だけ `TextInput`、rename は生 `String` でキャレット移動がない非対称も生じている。

### 3. モーダル分岐が 5 か所に同型展開（中）
create / rename / remove_modal / text_modal / quit_confirm の「フラグが立つ間は全キーを食う」制御が `event/mod.rs` と `event/handlers.rs` に同型で散在し、`continue;` 連鎖と優先順位の暗黙依存を生んでいる。

## 改善方針

- `HomeState` に `badges: MonitorSnapshot` を 1 フィールド持たせ `set_badges()` 1 本に集約。`is_running(path)` 等は `self.badges.running` へ委譲し、`live_count()` は `self.badges.live.len()` に。`terminal_pane` 側の 4 回 clone も 1 回に。
- 編集系は `active_input() -> &mut TextInput` のような共通アクセサにまとめ、ハンドラ側のキー処理を共通化する。最低でも rename を `TextInput` 化して create と同じ経路に載せ、専用メソッドと非対称を消す。
- モーダル/オーバーレイを 1 つの enum（例 `enum Overlay { None, Create, Rename, Remove, Text, QuitConfirm }`）に統合し、event ループ冒頭の `match overlay` 1 発で捌く。テキスト編集系オーバーレイはキーマップを共有する。

## 確認方法

- バッジ表示・各入力・モーダル操作が従来どおりであること（state/event テスト）。
- `cargo fmt` / `cargo clippy --all-targets -- -D warnings` / `cargo test`（カバレッジ 100% 維持）。
