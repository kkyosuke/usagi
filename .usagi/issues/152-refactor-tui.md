---
number: 152
title: refactor(tui): 左右ペインの再描画を列単位で分離し相互干渉をなくす
status: done
priority: medium
labels: [refactor, tui]
dependson: []
related: [151]
created_at: 2026-07-08T22:23:39.684825+00:00
updated_at: 2026-07-08T22:48:20.451820+00:00
---

## 背景 / 問題

home 画面の描画はモジュール分割（左=`ui/sidebar.rs`、右=`ui/panes.rs` 系、合成=`ui/mod.rs::render_frame`）としては綺麗に分離されている。しかし**端末への再描画の単位が「行まるごと」**になっており、左右ペインが出力レベルで結合している。

- `render_frame` は各ボディ行を `left_cell + " │ " + right_cell` の**1本の文字列に合成**する。
- 差分描画 `io/screen.rs::diff_frame`（Switch の `FramePainter::paint` / アタッチ中の `terminal/pane.rs::render` 双方が使用）は**行単位・行まるごと**で差分を取り、変わった行は `\x1b[{row};1H\x1b[2K`（行全体クリア）してから `left+│+right` を丸ごと書き直す。

このため:

- 左セルが変わった行は、同じ行の**右セルまで端末に再送**される（逆も同様）。左のマスコット点滅や作成/削除 skeleton アニメが、同じ行にある右ペイン（埋め込みターミナル）のセルを毎フレーム書き直し、`\x1b[2K` を伴うためチラつき・無駄な書き込みになる（内容は再計算されるので壊れはしない）。
- `render_frame` は毎描画で左右とも常に再生成し、ペイン単位の dirty 判定が無い。

**要望**: 左ペインを再描画するときに右ペインへ影響を出さない（逆も同様）。再描画の粒度を「行」ではなく「ペイン（列）」にする。

## 変更方針（列スコープ差分）

行合成をやめ、左右を最後まで別々の列として扱い、列スコープの差分書き込みにする。

1. `ui/mod.rs::render_frame`：左右を合成せず `left: Vec<String>` / `right: Vec<String>` を分けたまま writer に渡せる形にする（区切り `│` と列位置はメタ情報として持つ）。
2. `io/screen.rs`：列対応の差分描画を追加。`prev_left` / `prev_right` を別々に持ち、左が変わった行のみ `left_w` 幅に固定パディングした左セルを列1へ、右が変わった行のみ `right_w` 幅の右セルを列 `left_w+SEP_WIDTH+1` へ書く。`\x1b[2K`（行全体クリア）は使わず固定幅上書きにする。区切り `│` は初回/リサイズ時のみ描画。
3. `terminal/pane.rs::render`：上記列スコープ差分を使い、左のアニメで右ターミナルのセルが書き換わらないようにする。

## 注意点 / 例外

- `\x1b[2K`（clear-to-EOL）は列分離と両立しないため、固定幅パディング上書きへ置き換える（`clip_to_width` / `pad_to_width` を流用）。
- 両ペインをまたぐオーバーレイ（PR ポップアップの張り出し、レール折り畳み時に右へ移る inline create/rename、`:` パレット・各種モーダル）は従来どおり行単位/全体合成でよい。列分離は通常のボディ行に限定する。
- CJK 全角セルが左右境界を跨がないよう `left_w` のクリップ境界を厳密に保つ（`console::measure_text_width` ベースの既存幅計算を維持）。
- リサイズ時・初回描画・`FramePainter::reset` 経路のフリッカーフリー特性を壊さない。

## 受け入れ条件

- 左ペインのみ変化した場合、右ペインのセルが端末に再送されない（逆も同様）。左右どちらも変わらない行は従来どおりスキップ。
- 埋め込みターミナル表示中に左のマスコット/skeleton がアニメしても右ターミナル領域が書き換わらずチラつかない。
- 既存の描画結果（見た目）は不変。リサイズ・初回・reset のフリッカーフリー挙動を維持。
- クリーンアーキテクチャの依存方向を維持。

## テスト・確認

- カバレッジ 100% を維持（`cargo fmt` / `cargo clippy --all-targets -- -D warnings` / `cargo test`。pre-push で `cargo llvm-cov`）。
- `io/screen.rs` の既存 `diff_frame` テストに倣い列スコープ差分のユニットテストを追加（左のみ変化で右列に書き込みが出ない／右のみ変化で左列に出ない／両不変でスキップ／リサイズで下方のはみ出しをクリア）。

## 関連コード

- `src/presentation/tui/home/ui/mod.rs`（`render_frame` / `layout` / `SEP` / `SEP_WIDTH` / ボディ合成ループ）
- `src/presentation/tui/io/screen.rs`（`diff_frame` / `FramePainter`）
- `src/presentation/tui/home/terminal/pane.rs`（`render` / `diff_frame` 呼び出し）
- `src/presentation/tui/home/ui/sidebar.rs`（左ペイン生成）/ `ui/panes.rs`（右ペイン生成）

## 関連 issue

- #151（session 作成/削除アニメの再描画駆動）と同じ再描画パイプラインを触る。競合を避けるため #151 と同一セッションで順に対応すること。
