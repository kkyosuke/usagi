---
number: 90
title: feat(tui): 破壊的操作の確認を拡張（x での pane kill・Config 未保存 Esc 破棄）
status: todo
priority: medium
labels: [tui, review]
dependson: []
related: []
created_at: 2026-07-03T23:20:59.867645+00:00
updated_at: 2026-07-03T23:20:59.867645+00:00
---

UI/UX レビュー（2026-07 branch `usagi/ui`）由来。`close --force` の確認モーダルは別 PR で対応済み。残りの破壊的操作を同様にガードする。

## 1. 切替 `x` / 没入 `Ctrl-O x` が実行中 Agent のペインを確認なしで kill
`home/event/handlers.rs`（`x` ハンドラ）。`▶ running` の agent ペインも即座にシェルごと終了。切替では `x` が `c`（作成）の隣接キーで誤爆しやすい。
→ running/waiting のペインを閉じるときだけ y/n 確認を挟む（idle は即時のままでよい）。

## 2. Config の未保存変更が Esc / q で無警告破棄
`config/event/mod.rs`。`●` の未保存マークはあるが Esc 一発で全編集が黙って消える。Env Vars / Setup Commands をエディタで Ctrl-S（メモリ反映）した後でも Save 前に Esc すれば消える。
→ dirty 時の Esc に「Discard unsaved changes? y/n」を挟む。最低でも welcome へ戻る際に「Unsaved changes discarded」を通知。

## 受け入れ条件
- running/waiting ペインの `x` に確認が入り、idle は即時のまま。
- Config で未保存編集がある状態の Esc に確認/通知が入る。
- カバレッジ 100% 維持。
