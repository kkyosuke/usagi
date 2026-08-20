---
number: 711
title: feat(tui): Welcome の Open / Recent から別 workspace へ daemon を止めずに切り替えられるようにする
status: todo
priority: high
labels: [v2, tui, daemon, workspace]
dependson: [708, 709, 710]
related: [549]
created_at: 2026-08-20T23:34:15.105077+00:00
updated_at: 2026-08-20T23:34:15.105077+00:00
---

設計は [document/proposals/17-multi-workspace-daemon.md](../../document/proposals/17-multi-workspace-daemon.md) が正本。本 issue はその段階 4 と 6（正本の畳み込み）。#708 / #709 / #710 が前提。

## 問題・根拠

Welcome の Open / Recent は登録済み workspace を全件表示するのに、daemon が serve している 1 つ以外を選ぶと
typed workspace refusal になる。利用者から見えるのはこの文言だけである。

```text
cannot open /Users/…/usagi: this daemon does not serve the selected workspace;
this daemon serves the workspace /Users/…/AccelHack.
Stop it with `usagi daemon stop`, then start usagi in /Users/…/usagi.
```

## 方針

- 選んだ workspace の subtree の locator へ接続し、居なければその workspace で cold start する（[03-tui.md#workspace-の選択と-daemon](../../document/03-tui.md#workspace-の選択と-daemon) の表を更新）。
- **fence refusal path は削除しない**。stale locator・手書き path・digest 衝突で誤った daemon に届く経路は残るため、refusal の提示と test は保持する。
- 1 process 1 daemon 接続の契約は不変。離脱時に前 workspace の port・pump・worker を落としてから次へ接続する（[workspace の離脱と終了](../../document/03-tui.md#workspace-の離脱と終了)）。
- 正本を畳み込む: [05-daemon.md](../../document/05-daemon.md) の daemon data directory と 2 段 fence、[04-ipc.md](../../document/04-ipc.md) の workspace fence 末尾（「別 workspace を同時に扱うことはできない」）、[03-tui.md](../../document/03-tui.md) の workspace 選択。proposal 側は畳み込み済みとして状態を更新する。

## 受入条件

- workspace A を開いて Agent を live にしたまま Welcome へ戻り、workspace B を開いて操作でき、A の Agent が生存していることを実 PTY E2E で確認する（[重い E2E の直列化](../../document/06-conventions.md#重い-e2e-の直列化)の列に載せる）。
- 別 workspace の title の下に別 workspace の session 一覧を出さない（#549 の受入条件を維持する）。
- fence refusal のときは従来どおり画面に留まり、折り返した notice に理由と復帰手順を出す。
- カバレッジ 100% を維持する。
