---
number: 490
title: fix(tui): Ctrl-O leader と global control chord の優先順を統一する
status: in-progress
priority: medium
labels: [review, v2, tui, input]
dependson: []
related: [224, 303, 305, 408]
parent: 453
created_at: 2026-07-20T12:06:51.145630+00:00
updated_at: 2026-07-21T12:28:12.684404+00:00
---

## 問題・影響

root/v2 の `src/runtime/tui.rs::CrosstermTerminal::read_key` は global `control_key` を `LiveInputClassifier::classify` より先に処理する。`Ctrl-O` leader 待機中の Ctrl-C/Q/D が classifier を通らず global action を起こし、leader state も残るため、次 key の解釈が順序依存で不整合になる。

## 成立条件 / 再現フロー

live pane で Ctrl-O の後に Ctrl-C、Ctrl-Q、Ctrl-D を押し、続けて通常 key を送る。global quit/unregister action が即発火するか、残存 leader が後続 key を飲み込む。raw byte/semantic event でも差が出る。

## 対象責務と非対象

pending leader と global chord の優先順位、leader consume/reset、raw/semantic event parity を対象とする。key binding 自体の再設計、#287 の Ctrl-A UX、terminal reconnect は非対象。

## 受入条件

- [ ] pending leader がある場合の全 follow-up を classifier の単一 policy で consume/resolve/reset してから global action を判断する。
- [ ] leader なしの Ctrl-C/Q/D は従来の global semantics を維持する。
- [ ] timeout、release、unknown follow-up、raw/semantic control で leader state が残留しない。
- [ ] key ordering contract を composition と pure classifier で二重実装しない。

## 必須回帰テスト

leader後 Ctrl-C/Q/D、leaderなし global chord、raw/semantic byte、timeout境界、release/auto-repeat、unknown follow-up→次 key を table test と terminal adapter test で固定する。

## docs / 移行影響

`document/03-tui.md` の live prefix/global shortcut 優先順位を更新する。永続 migration はない。
