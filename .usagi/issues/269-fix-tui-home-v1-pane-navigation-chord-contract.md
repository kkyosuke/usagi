---
number: 269
title: fix(tui): Home の v1 pane navigation chord contract を固定する
status: done
priority: high
labels: [tui, bug, parity, pane]
dependson: []
related: [258, 267]
created_at: 2026-07-13T00:57:21.021155+00:00
updated_at: 2026-07-13T00:59:36.859467+00:00
---

## 目的

v2 Home controller の management input を v1 の pane navigation 契約に揃える。Closeup からの `Ctrl-O` は Switch へ戻り、`Ctrl-A` は新規 session 作成を開始せず active target の Closeup action overlay を開く。Switch での `Ctrl-A` は従来どおり新規 session 作成フォームを開く。

## 調査根拠

- #267 は `HomeProjection` の Chrome 風 tab strip と空状態を controller renderer に追加済みだが、キー遷移は変更していない。
- #258 は root-first sidebar と controller runtime 接続を扱う in-progress issue で、right-pane/tab layout は対象外である。
- 現行 controller は `Ctrl-A` を Home mode に関係なく `CreateSession` に送る。また management input に `Ctrl-O` の語彙がない。
- v1 は Closeup/immersive から Switch へ戻る pane navigation を持ち、Closeup action surface と新規 session 作成を別 scope として扱う。

## スコープ

- `AppKey` / management-input classifier / reducer に Ctrl-O と mode-aware Ctrl-A を追加する。
- Closeup の Ctrl-O→Switch、Closeup の Ctrl-A→`Overlay::Closeup`、Switch の Ctrl-A→`Overlay::CreateSession` を純粋 reducer tests と parity scenario で固定する。
- v2 正本 `document/03-tui.md` に key transition と overlay scope を記載する。

## 対象外

- #258 の runtime composition / sidebar row migration。
- #267 の tab chrome / empty-state layout。
- daemon、PTY、terminal input passthrough、Closeup command registry の変更。

## 完了条件

- Closeup で Ctrl-A が create form を開かず、Closeup overlay を開く。
- Ctrl-O が Closeup から Switch へ遷移し、Switch では no-op とする。
- Switch の Ctrl-A は create form を維持する。
- 入力 classifier と reducer の table-driven regression tests、および実装済み仕様が同じ PR に含まれる。
