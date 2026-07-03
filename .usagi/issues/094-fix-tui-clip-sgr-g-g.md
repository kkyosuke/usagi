---
number: 94
title: fix(tui): 描画細部の改善（clip の SGR 未クローズ・ホイールスクロール・スクロール位置表示・g/G ジャンプ）
status: todo
priority: low
labels: [tui, review]
dependson: []
related: []
created_at: 2026-07-03T23:21:48.806395+00:00
updated_at: 2026-07-03T23:21:48.806395+00:00
---

UI/UX レビュー（2026-07 branch `usagi/ui`）由来。低優先の描画/操作の細部。

## 項目
1. **`clip_to_width_cow` がスタイルを閉じない**（`widgets/mod.rs`）: スタイル付き文字列を途中で切ると、コピー済みの SGR を閉じずに `…` を付けて終わる。兄弟関数（`overlay_block` / `slice_from_width`）は `RESET` を付けるのに、`clip_to_width` 経由（`boxed` のパディング＋右罫線、`markdown_row`）は開きっぱなしの色が後続へ滲みうる（reverse / 背景色付きスパンで顕在化）。→ ESC を 1 つでもコピーしたら末尾に `RESET` を付与。
2. **マウスホイールをペインスクロールへ配線**（`home/event/mod.rs` が `Input::Scroll(_)` を全捨て）: プレビュー/diff/テキストモーダルはキーでスクロールできる面なので、右ペインが preview/diff のときはホイールを `preview_scroll_*` / `diff_scroll_*` へ配線（ペイン座標判定インフラは既存）。scrollback 露出防止の設計意図は維持。
3. **スクロール位置表示の統一**（`home/ui/panes.rs` の `sidebar_scroll`）: サイドバーだけ位置表示がない。preview/diff は `start-end/total`、dir_picker は `… N more` がある。→ `… (+N)` かヘッダに `(3-6/12)` 相当を追加。
4. **先頭/末尾ジャンプ**: Home/End は作成/メモに割当済みで、リストの先頭/末尾へ飛べない。→ 未使用の vim 流 `g`/`G` を割当。

## 受け入れ条件
- クリップ時に色が後続へ滲まない（styled 行クリップの reset テスト追加）。
- preview/diff でホイールが効く。サイドバーに位置表示。`g`/`G` で先頭/末尾へ。
- カバレッジ 100% 維持。
