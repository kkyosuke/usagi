---
number: 226
title: feat(tui): dispatch modal commands through the registry
status: done
priority: high
labels: [tui]
dependson: [225]
related: []
created_at: 2026-07-12T13:33:33.553602+00:00
updated_at: 2026-07-12T13:39:54.043112+00:00
---

## 目的

Overview / Closeup modal の候補、completion、dispatch を usecase registry の単一情報源へ統合し、controller の effect port を通じて安全に request を発行する。

## 受け入れ条件

- Overview と Closeup の候補表示・入力補完・dispatch が各 usecase registry の metadata を正本にする。
- Overview の workspace command、Closeup の terminal / agent / close が controller effect として 1 回だけ dispatch される。
- 不正入力、root target の close、二重 Enter は request を発行しない。
- reducer + fake port と modal render のテストで A-DISPATCH-1 の境界を固定し、既存 ANSI/CJK・small terminal・背景合成テストを維持する。
- daemon wire / IPC / session mutation / terminal attach / src/main.rs / crossterm mapping は対象外とする。

## 参照

- `document/proposals/06-tui-v1-parity.md` の `A-DISPATCH-1`
- #225 を開始条件とするが、未マージ API には依存しない。
