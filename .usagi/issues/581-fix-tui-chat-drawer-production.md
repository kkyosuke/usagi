---
number: 581
title: fix(tui): chat drawer の production 入力優先順位を修正する
status: done
priority: high
labels: [tui, bug, input, agent]
dependson: []
related: [578, 579, 580]
created_at: 2026-07-29T23:35:29.586584+00:00
updated_at: 2026-07-29T23:45:51.259536+00:00
---

## 背景

Workspace Agent drawer に live root Agent が選択されていると、production loop が drawer の予約キーより先に PTY 転送し、picker 操作や close が Agent へ漏れる。New shortcut も NextTab の Ctrl-O Ctrl-N と衝突しない leader + plain key に分離する必要がある。

## 対象

- New を Ctrl-O → plain n に割り当て、Ctrl-O Ctrl-N の NextTab と bare n の PTY 入力を維持する。
- frontmost overlay を優先しつつ、Switch / Closeup / live pane から drawer を開いて New picker を表示する。
- production shell loop で drawer 予約操作を PTY 転送前に一意に処理し、picker の ↑↓ / Enter / Esc と picker 閉状態の Esc を PTY に送らない。
- picker Choosing 中の [ New ] 再クリックを consume し、mouse-up を背景 pane / focus に漏らさない。
- root / managed scope と drawer close 後の focus 復元を維持する。
- footer key hints と document/03-tui.md を実装に合わせる。

## 受入条件

- [x] LiveInputClassifier は Ctrl-O plain n を New action、Ctrl-O Ctrl-N を NextTab、bare n を PTY と分類する。
- [x] production shell loop test で live root Agent 選択中も Ctrl-O n から picker を開け、↑↓ / Enter / Esc は PTY へ 1 byte も送らず、launch / cancel / drawer close が期待どおり動く。
- [x] picker 閉状態の通常 Agent 入力は PTY に届き、Esc と Ctrl-O Ctrl-G は drawer を閉じる。
- [x] [ New ] の double-click 相当の 2 回 mouse-down で launch effect は 0、背景 managed pane の focus / tab は不変である。
- [x] shipping PTY E2E は live Agent 存在中の Ctrl-O n New picker 経路を通す。
- [x] document/03-tui.md と drawer footer は New、close、picker 操作を正しく案内する。
