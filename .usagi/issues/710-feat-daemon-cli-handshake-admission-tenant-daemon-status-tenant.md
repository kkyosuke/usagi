---
number: 710
title: feat(daemon,cli): handshake admission を tenant 解決にし daemon status に tenant 一覧を出す
status: todo
priority: high
labels: [v2, daemon, ipc, cli, workspace]
dependson: [708, 709]
related: []
created_at: 2026-08-20T23:33:56.462699+00:00
updated_at: 2026-08-21T00:10:32.366366+00:00
---

設計は [document/proposals/17-multi-workspace-daemon.md](../../document/proposals/17-multi-workspace-daemon.md) が正本。本 issue はその段階 3。#708 / #709 が前提。

## 問題・根拠

`ServerProtocol` は `workspace_root` を 1 本だけ持ち、`workspace_admission` はそれとの一致で admit / refuse を決める。
tenant registry ができても、handshake がこの単数の root を見ている限り別 workspace の client は拒否される。

## 方針

申告（`unbound` / `bound` / `selected`）とその決定順（[04-ipc.md#workspace-fence](../../document/04-ipc.md#workspace-fence)）は変えず、daemon 側の判定を tenant 解決にする。

| 申告 | 判定 |
|---|---|
| `selected` | adopt 済み tenant と完全一致なら admit。未 adopt ならその場で adopt して admit |
| `bound` | adopt 済み tenant のいずれかの配下なら admit（最長一致でその tenant へ解決）。どれにも属さなければその root を adopt して admit |
| `unbound` | 変更なし |

- refusal は残す。理由は「fence を他 process が所有」「root が非 UTF-8」「tenant 上限」の 3 つで、いずれも **workspace 単位**の拒否として返す。message は「この workspace は別の daemon が所有している（pid N）」に変える。
- `ServerHello` はその接続が解決した tenant の root を返し、client 側の fence 検証は現行のまま機能させる。
- adopt 済み tenant 数の上限を設ける。到達時は typed refusal に復帰手順（未使用 workspace の retire、daemon 再起動）を載せる。
- `usagi daemon status` は adopt 済み tenant を列挙する（root・session 数・live runtime 数）。`stop` / `restart` / `replace` の対象は machine の daemon 1 つのままで、`stop` の live runtime 判定は全 tenant を対象にする。

## 受入条件

- `selected` / `bound` の tenant 解決（session worktree・subdirectory・未 adopt の workspace）が unit test で固定される。
- 別 workspace の cwd から `usagi session list` 相当が動き、その workspace の一覧だけを返す結合テストが通る。
- 先に別 process が fence を握った workspace を選ぶと、その workspace だけが refusal になり、既存 tenant の接続は継続する。
- `daemon status` が複数 tenant を列挙する。
- カバレッジ 100% を維持する。
