---
number: 337
title: fix(tui): ターミナル resize 時に画面が再描画されず表示が崩れたままになる
status: done
priority: medium
labels: [tui, bug]
dependson: []
related: []
created_at: 2026-07-10T13:31:34.921092+00:00
updated_at: 2026-07-10T13:56:05.585416+00:00
---

# fix(tui): ターミナル resize 時に画面が再描画されず表示が崩れたままになる

## 再現手順

1. `usagi` を起動しホーム画面（選択/集中）または welcome / open / config などの管理画面を表示する。
2. ホストターミナルのウィンドウサイズを変更する。
3. 画面が新しいサイズで再描画されず、崩れた表示・古いレイアウトのまま残る。次のキー入力後も、変化しなかった行が差分描画にスキップされて崩れが残ることがある。

（没入＝埋め込みターミナルは `last_geo` 比較で PTY リサイズ＋再描画済みだが、差分基準 `prev` を無効化しないため一部の行が崩れたまま残り得る。）

## 原因

TUI は crossterm のイベントストリームを使わず console ベースで入力を読むため、`Event::Resize` に相当するイベントが存在しない。resize は SIGWINCH → EINTR の副作用としてしか観測されず、各所で握りつぶされている:

- `io/term_reader.rs` の `TermKeyReader::next_key` は、resize で発生する EINTR 由来の偽 `Key::CtrlC` を検出すると**読み捨てて再ブロック**する（「次の実キーで再描画される」前提）。ブロッキング読みの画面は次のキーまで再描画されない。
- `home/event/mod.rs` の `event_loop` は毎パス `term.size()` を読むが、**`skip_paint` 判定にサイズ変化が入っていない**ため、静かな選択（Overview）ではアイドルティックが来ても再描画をスキップする。
- `io/screen.rs` の `FramePainter`（#65 の再描画コアレス、#152 の列単位差分）は**直前フレーム `prev` を差分基準として保持し続ける**。resize すると端末側の表示は再フロー・切り詰めで実態が変わるのに `prev` は無効化されないため、次の描画でも「変化していない行」がスキップされ、崩れが画面に残る。
- `home/terminal/pane.rs`（没入）も独自の `prev` を持ち、resize 時に PTY はリサイズするが `prev` を破棄しない。

## 対応方針

resize 検知時に**全画面クリア＋フル再描画（差分キャッシュの無効化）**を行う:

1. `FramePainter` が flush ごとに端末サイズを記録し、変化していたら `prev` を破棄（→ 次の描画が `\x1b[2J` 全画面クリア＋全行描画になる）。全画面共通の一元対応。
2. `home/event/mod.rs`: サイズ変化を検出したら `force_paint` を立て、`skip_paint` に食われないようにする（アイドルティック/EINTR で覚醒した次のパスで確実にフル再描画）。
3. `TermKeyReader::next_key`: resize アーティファクト（サイズ変化を伴う偽 CtrlC）を読み捨てて再ブロックする代わりに `Key::Unknown` として返し、ブロッキング読みの画面（welcome / open / config / gallery など）もループ先頭の再描画に到達させる（各画面は未知キーを無視して再描画するだけなので安全）。
4. `home/terminal/pane.rs`: ジオメトリ変化時に差分基準 `prev` を破棄してフル再描画する。
5. `io/signals.rs`: SIGWINCH に no-op ハンドラを登録し、埋め込みペインを一度も開いていない起動直後でも resize がブロッキング読みを EINTR で覚醒させるようにする（クロスターム側のハンドラ登録後と同じ挙動に揃える。signal-hook は既存ハンドラを連鎖呼び出しするため共存可）。

テストは注入済みの seam（純ロジック関数・`FramePainter` のユニットテスト・スクリプト化 `KeyReader`）で追加し、カバレッジ 100% を維持する（`term_reader.rs` / `signals.rs` / `pane.rs` は実 IO のため計測対象外）。

## 関連

- #65 再描画コアレス（`FramePainter` 差分描画）
- #152 左右ペインの列単位差分
