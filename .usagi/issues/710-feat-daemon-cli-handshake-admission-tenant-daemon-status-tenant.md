---
number: 710
title: feat(daemon,cli): handshake admission を tenant 解決にし daemon status に tenant 一覧を出す
status: done
priority: high
labels: [v2, daemon, ipc, cli, workspace]
dependson: [708, 709]
related: []
created_at: 2026-08-20T23:33:56.462699+00:00
updated_at: 2026-08-21T02:20:26.853454+00:00
---

設計は [document/proposals/17-multi-workspace-daemon.md](../../document/proposals/17-multi-workspace-daemon.md) が正本。本 issue はその段階 3。#708 / #709 / #713 が前提。

## 問題・根拠

`ServerProtocol` は `workspace_root` を 1 本だけ持ち、`workspace_admission` はそれとの一致で admit / refuse を決める。
tenant registry ができても、handshake がこの単数の root を見ている限り別 workspace の client は拒否される。

## 方針

申告（`unbound` / `bound` / `selected`）とその決定順（[04-ipc.md#workspace-fence](../../document/04-ipc.md#workspace-fence)）は変えず、daemon 側の判定を tenant 解決にする。

| 申告 | 判定 |
|---|---|
| `selected` | canonical 化して adopt する（保持済みならそれを使う） |
| `bound` | 保持中の workspace の最長一致へ解決する。どれにも属さない path は拒否（directory だけでは workspace root を名指せないので adopt しない） |
| `unbound` | 起動 workspace を答える（workspace resource を読まない） |

- 解決は **generation fence の後**に走る。逆順だと、別 generation を目指した client が拒否される過程で workspace を adopt させられる。
- refusal は残す。理由は「fence を他 process が所有」「root が解決できない」「tenant 上限」の 3 つで、いずれも **workspace 単位**の拒否。
- fence 本体（`workspace_admission`）は解決の後段にそのまま残り、誤った endpoint に届いた client の backstop になる。
- adopt は client の handshake 内で走るため、fence の待ちは起動時の 2 秒ではなく 200ms に固定する（pre-handshake deadline に当たると typed refusal が切断として観測される）。

## 受入条件

- `selected` / `bound` の tenant 解決（session worktree・subdirectory・未 adopt の workspace・存在しない path）が unit test で固定される。✅
- 1 つの daemon が 2 つ目の workspace を adopt し、それぞれが自分の state subtree を持つことを結合テストで確認する。✅
- 他 process が fence を持つ workspace はその workspace だけが typed refusal になり、保持中の tenant の接続は継続する。✅
- カバレッジ 100% を維持する。✅

`daemon status` への tenant 一覧表示は #714 へ切り出した（現在保持している集合は daemon にしか分からず、read-only な IPC の追加が要るため）。
