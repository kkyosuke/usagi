---
number: 154
title: fix(tui): Ctrl+C 連打による usagi の誤終了を防ぐ
status: done
priority: medium
labels: [fix, tui]
dependson: []
related: []
created_at: 2026-07-09T22:44:19.192794+00:00
updated_at: 2026-07-09T22:58:08.434469+00:00
---

## 背景

TUI では「agent（PTY にアタッチ中の Claude Code など）を閉じる/中断する」操作が Ctrl+C で、「usagi 自体を終了する」操作も Ctrl+C。agent を閉じるつもりで Ctrl+C を連打すると、agent 終了 → ペインが閉じてトップレベル（選択/集中）に戻る → 連打の続きが usagi 側ハンドラに刺さり、usagi ごと終了してしまう事故が起きる。

現状の緩和が効いていない原因は2点:

1. 終了確認モーダルが **2回目の Ctrl+C を「はい」として受理**していた（`event/mod.rs`）。連打がそのままモーダルを開いて確定してしまう。
2. 生きたセッションが無い（＝最後の agent を殺した直後）と Ctrl+C は **確認なしで即終了**する。

## 変更方針

- **モーダル内の Ctrl+C / Ctrl+Q を no-op 化**。確定は `y`/`Y`/`Enter` のみ、キャンセルは `n`/`N`/`Esc`。純粋な Ctrl+C 連打ではモーダルを確定できないようにする。
- **ペイン離脱直後の one-shot グレース**。agent を閉じた/detach した/ズームアウトした直後にトップレベルへ戻ったとき、次の 1 回の Ctrl+C を握りつぶしてヒントを出す（連打の反射的な余りを吸収）。event ベースの一発消費なのでハングしない。
- idle 時の即終了は据え置き（テストハーネスの終了子が Ctrl+C に依存するため。グレース＋モーダル no-op で連打事故は実用上防げる）。

## 影響

- live monitor を使うテスト（`clicks.rs` / `quit_modal.rs`）は「フォールバック Ctrl+C で確定」に依存していたため、明示確定キーへ更新する。
- ドキュメント（design のキー操作・終了挙動）を更新する。
