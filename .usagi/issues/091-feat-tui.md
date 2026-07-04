---
number: 91
title: feat(tui): 在席コマンド結果のインライン表示とマウス専用機能のキーボード代替
status: todo
priority: medium
labels: [tui, review]
dependson: []
related: []
created_at: 2026-07-03T23:21:09.976320+00:00
updated_at: 2026-07-03T23:21:09.976320+00:00
---

UI/UX レビュー（2026-07 branch `usagi/ui`）由来。フィードバックとアクセシビリティ。

## 1. 在席で実行したコマンドの結果・エラーが見えない
`home/state/mod.rs` の `record_response` は log へ積むだけで、在席の右ペイン（`focus_menu_body` / `focus_prompt_body`）はレスポンスを描かない。拒否理由（`"…" is not available here`、close の dirty 拒否、`ai` の coming soon 等）が**次に `:` パレットを開いたときにしか見えず**、体感は「押したのに何も起きない」。05-overlays.md もこの弱さを認めている。
→ 在席右ペイン（またはフッター直上）に直近レスポンス 1 行の帯を出す、もしくは数秒で消えるトースト行。

## 2. マウス専用機能にキーボード代替がない
PR ポップアップ（バッジクリックのみ）、タブメニュー＝リネーム/削除（右クリックのみ）、アップデート確認（マスコット左クリックのみ）。マウスレポート非対応端末（Apple Terminal.app 等）では **PR 番号一覧・タブのリネーム・画面内アップデートに到達できない**（04-keys/05-overlays が自認）。
→ パレットコマンド（`pr` / `tab rename` / `update`）かキー割当（在席で `P` 等）の鍵盤経路を用意。

## 受け入れ条件
- 在席のコマンド結果/エラーがその場で見える。
- マウスなしで PR 一覧・タブrename/削除・アップデートに到達できる。
- カバレッジ 100% 維持。
