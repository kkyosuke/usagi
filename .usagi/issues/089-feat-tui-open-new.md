---
number: 89
title: feat(tui): Open 一覧の縦スクロールと New 入力行のクリップ／水平スクロール
status: done
priority: medium
labels: [tui, review]
dependson: []
related: []
created_at: 2026-07-03T23:20:43.890406+00:00
updated_at: 2026-07-03T23:20:43.890406+00:00
---

UI/UX レビュー（2026-07 branch `usagi/ui`）由来。レイアウト堅牢化 2 件。

## 1. Open 一覧に縦スクロールがない
`src/presentation/tui/open/ui.rs` の `list_lines` が全行を無条件出力し、`io/screen.rs` の `diff_frame` は height 超過を防御しない。1 ワークスペース 2 行なので 24 行端末では 7〜8 件で溢れ、最下行に重ね書きされ画面が崩れる。カーソルが画面外に出ると選択位置も見えない。dir picker（`MAX_ROWS=10`＋`… N more`）やホームのサイドバー（#568）には同等機構があるのに Open だけ未対応。
→ dir picker と同じカーソル追従ウィンドウイング＋`… N more` を導入。

## 2. New 入力行が幅クリップされず長い URL で崩れる
`new/ui.rs` の `input_line` に幅制限がなく、端末幅を超える URL/パスで行が折り返して 1 行 1 要素前提の差分描画がズレ、画面全体が崩れる。長い入力ではキャレットも見切れる。Config は全行クランプ、welcome は `clip_to_width`、Open はパス cap 済みで、**New だけ抜けている**。
→ 各行を `clip_to_width`。理想は入力欄のキャレット追従水平スクロール。

## 受け入れ条件
- 多数登録・低い端末でも Open が崩れずスクロールで全件到達できる。
- 長い URL を入力しても New が崩れず、キャレットが見える。
- カバレッジ 100% 維持。
