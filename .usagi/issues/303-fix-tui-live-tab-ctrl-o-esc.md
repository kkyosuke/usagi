---
number: 303
title: fix(tui): live tab の入力優先と Ctrl+O・Esc の脱出契約を統一する
status: todo
priority: high
labels: [tui, terminal, input, bug]
dependson: []
related: [279, 282]
created_at: 2026-07-15T00:28:01.491437+00:00
updated_at: 2026-07-15T00:29:54.899426+00:00
---

## 目的

live terminal / Agent tab を表示中、通常入力は常に選択中 tab へ 1 回だけ渡し、管理ショートカットだけを TUI が予約する。Ctrl+O と Esc は prefix・モーダル・tab 状態を残さず、予測可能に管理面へ戻れるようにする。

## 背景

現在は `LiveInputClassifier` が Ctrl+O leader と follow-up を処理し、controller / legacy runtime の入力経路も並存している。tab がある場面で文字入力、leader、Esc の所有者と優先順位が一貫していることを runtime まで固定する必要がある。

## 受け入れ条件

- 選択中 live tab があるとき、予約済み操作以外の key / paste / raw bytes は PTY に一度だけ転送される。
- Ctrl+O は terminal に送られず、管理操作へ遷移する。prefix 待機中の Ctrl+O / Esc は prefix を取消し、意図しない follow-up action を起こさない。
- Esc は最前面の overlay / prefix / tab 操作状態だけを閉じ、PTY へ漏れず、選択中 tab と session identity を壊さない。
- tab 無し・pending・ended・再接続時も同じ優先順位で安全に縮退する。
- classifier、reducer、実行時 dispatch を通す回帰テストを追加し、shortcut の bytes が二重送信されないことを検証する。

## 関連

- #279 は controller の prefix / tab-gating 投影を完了済み。本 issue は実行時の入力所有と脱出契約を対象にする。
- #282 は session-scoped tab registry の所有境界を扱う。
