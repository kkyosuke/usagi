---
number: 94
title: fix(tui): 描画細部の改善（clip の SGR 未クローズ・ホイールスクロール・スクロール位置表示・g/G ジャンプ）
status: in-progress
priority: low
labels: [tui, review]
dependson: []
related: []
created_at: 2026-07-03T23:21:48.806395+00:00
updated_at: 2026-07-10T03:29:51.779049+00:00
---

UI/UX レビュー（2026-07 branch `usagi/ui`）由来。低優先の描画/操作の細部。

## 項目
1. ✅ **`clip_to_width_cow` がスタイルを閉じない**（`widgets/mod.rs`）: スタイル付き文字列を途中で切ると、コピー済みの SGR を閉じずに `…` を付けて終わる。→ ESC を 1 つでもコピーしたら末尾に `RESET` を付与。**対応済み**。
2. ✅ **マウスホイールをペインスクロールへ配線**（`home/event/mod.rs`）: 右ペインの diff / Markdown プレビュー / テキストモーダルが開いているとき、ホイールを `diff_scroll_*` / `preview_scroll_*` / `text_modal_scroll_*` へ配線（`scroll_open_surface`）。それ以外では従来どおり読み捨て、scrollback 露出防止を維持。**対応済み**。
3. **スクロール位置表示の統一**（`home/ui/panes.rs` の `sidebar_scroll`）: サイドバーだけ位置表示がない。preview/diff は `start-end/total`、dir_picker は `… N more` がある。→ `… (+N)` かヘッダに `(3-6/12)` 相当を追加。**未対応**（サイドバーの render + scroll 計算 + クリック hit-test の協調変更＋オーバーフロー境界での 1 行 CLS を伴うため、別途慎重に対応）。
4. ✅ **先頭/末尾ジャンプ**: Home/End は作成/メモに割当済みで、リストの先頭/末尾へ飛べない。→ vim 流 `g`/`G` を 選択(Switch) に割当。**対応済み**。

## 受け入れ条件
- クリップ時に色が後続へ滲まない（styled 行クリップの reset テスト追加）。✅
- preview/diff でホイールが効く。✅ ／ サイドバーに位置表示（項目3・未対応）／ `g`/`G` で先頭/末尾へ。✅
- カバレッジ 100% 維持。✅（済んだ項目について）
