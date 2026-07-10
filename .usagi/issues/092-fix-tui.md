---
number: 92
title: fix(tui): 端末リサイズ追従と再描画の堅牢化
status: done
priority: medium
labels: [tui, review]
dependson: []
related: []
created_at: 2026-07-03T23:21:21.606469+00:00
updated_at: 2026-07-10T20:58:52.062055+00:00
---

UI/UX レビュー（2026-07 branch `usagi/ui`）由来。

## 問題
1. welcome / config / new / open は**キーを押すまで旧サイズのまま**（ブロッキング read）。
2. home も EINTR で起きるが `skip_paint` が状態変化しか見ないため、リサイズだけでは再描画されないことがある（`home/event/mod.rs`）。
3. サイズが変わっても `painter.reset()` されないため、端末側がリサイズで reflow/クリップした後に「差分なし」と判断された行が壊れたまま残り得る。

## 対応
- 各ループで `(height,width)` を前回値と比較し、変化時は `painter.reset()` ＋強制 repaint。
- home は `skip_paint` の条件に size 変化を追加。
- key-only 画面は `read_key_timeout` ベースへ寄せて低コストで追従。

## 受け入れ条件
- 各画面で端末リサイズ後、キー入力なしで正しく再レイアウトされる。
- リサイズ後に壊れた行が残らない。カバレッジ 100% 維持。

## 対応結果

問題 1〜3 の大部分は #166（PR #711）で解消済み:

- 問題 3: `FramePainter::invalidate_on_resize` が flush ごとに端末サイズを記録し、変化時に差分基準を破棄 → 次の描画は全画面クリア＋フル再描画（全画面に一元適用。没入ペインも同様）。
- 問題 2: home のイベントループに `size_changed` を追加し、サイズ変化で `force_paint` を立てて `skip_paint` を解除。
- 問題 1: 起動時に no-op の SIGWINCH ハンドラを登録し、resize がブロッキング read を EINTR で起こす。キー読み取り層は resize アーティファクトを `Key::Unknown` として返し、各画面のループはループ先頭で新サイズのまま再描画する。「`read_key_timeout` ベースへ寄せる」代替案は不要になった（EINTR 覚醒はポーリングなしでキー入力なしの追従を満たし、アイドル時のコストもゼロのまま）。

本 issue では残っていた最後の穴を塞いだ: `animated_read`（welcome / config / new / open がバックグラウンドインストールのオーバーレイをアニメさせる間のタイムアウト読み）では resize がただのティックに化け、旧サイズのフレームを描き直すだけでキーを押すまで再レイアウトされなかった。ティックごとに端末サイズを前回値と比較し、変化時はブロッキング read と同じ `Key::Unknown` を返して呼び出し元ループに新サイズで再レイアウトさせるようにした（サイズ取得を注入可能にしてユニットテストで両分岐をカバー）。
