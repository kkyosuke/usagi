---
number: 711
title: feat(tui): Welcome の Open / Recent から別 workspace へ daemon を止めずに切り替えられるようにする
status: done
priority: high
labels: [v2, tui, daemon, workspace]
dependson: [708, 709, 710]
related: [549]
created_at: 2026-08-20T23:34:15.105077+00:00
updated_at: 2026-08-21T02:56:13.279093+00:00
---

設計は [document/proposals/17-multi-workspace-daemon.md](../../document/proposals/17-multi-workspace-daemon.md) が正本。本 issue はその段階 4（正本の畳み込みを含む）。#708 / #709 / #713 / #710 が前提。

## 問題・根拠

Welcome の Open / Recent は登録済み workspace を全件表示するのに、daemon が serve している 1 つ以外を選ぶと
typed workspace refusal になっていた。

```text
cannot open /Users/…/usagi: this daemon does not serve the selected workspace;
this daemon serves the workspace /Users/…/AccelHack.
Stop it with `usagi daemon stop`, then start usagi in /Users/…/usagi.
```

## 方針

- 別 workspace を選んでもそのまま開く。接続先の daemon は同じなので、切り替えは handshake の再実行で済む（#710 の tenant 解決が adopt を行う）。
- **fence refusal path は削除しない**。別 process（別 mode・別 build の daemon）が対象 workspace の fence を持つ場合は、その workspace だけが拒否される。
- 1 process 1 daemon 接続の契約は不変。離脱時に前 workspace の port・pump・worker を落としてから開き直す。
- 正本を畳み込む: [05-daemon.md](../../document/05-daemon.md)（tenant registry、2 段 fence）、[04-ipc.md](../../document/04-ipc.md)（workspace の解決と refusal 理由）、[03-tui.md](../../document/03-tui.md)（workspace の選択と daemon、離脱と再接続）。

## 受入条件

- workspace A を開いて runtime を live にしたまま Welcome へ戻り、workspace B を開いて操作でき、A の runtime が生存していることを実 PTY E2E で確認する。✅ `real_pty_switching_workspaces_adopts_the_second_and_keeps_the_first_live`（daemon を停止してから TUI を起動するので、B の adopt は切り替えの瞬間に起きる）
- 別 workspace の title の下に別 workspace の session 一覧を出さない（#549 の受入条件を維持する）。✅ 切り替え後の画面は B 自身の名前で描画される
- fence を他 process が持つ workspace を選んだときは従来どおり拒否される。✅ #710 の結合テスト
- カバレッジ 100% を維持する。✅
