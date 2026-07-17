---
number: 342
title: feat(integration): managed session から agent／terminal 実行までを daemon root で接続する
status: todo
priority: high
labels: [integration, daemon, session, agent, terminal, e2e]
dependson: [263, 264, 268]
related: [251, 252, 253, 255, 265]
parent: 227
created_at: 2026-07-13T01:35:00.000000+00:00
updated_at: 2026-07-13T01:35:00.000000+00:00
---

## 背景・根拠

#263、#264、#268 はそれぞれ TUI agent launch、generic terminal vocabulary、session lifecycle runtime の契約と個別 runtime を導入した。しかし現行の合成 root は `SessionRuntime` だけを全 IPC connection で共有し、terminal request は generic `dispatch` に戻る。agent request も durable runtime／terminal completion へ接続されず `Accepted` を返すだけである。

したがって、`usagi` 起動後に daemon が managed session を作成し、その stable scope で agent と generic terminal を実行する最小操作フローはまだ成立しない。各 feature の reducer や fake runtime を再実装せず、合成 root と runtime 間の ownership／completion 境界を接続する integration slice が必要である。

## 目的

`usagi` 起動 → managed session 作成 → agent 実行 → generic terminal 実行を、daemon が唯一の state／process owner として通す。TUI・CLI・MCP は daemon IPC client のままであり、local worktree／PTY／agent process fallback を持たない。

## 実装開始条件

次の既存成果を消費し、同じ contract を別型・別 store として複製しない。

| 依存 | 消費する責務 | integration が補う接続 |
|---|---|---|
| #268 | durable session、scope identity、worktree effect | available session の fenced scope を agent／terminal launch に渡す |
| #264 | `TerminalOwner`、generic terminal request、coordinator | root で一つの実 terminal owner を組み、connection handler と PTY lifecycle に渡す |
| #263 | agent launch effect と pending/attach projection | runtime completion を accepted/final と terminal attach へ配送する |

実装を開始する前に、上の各 dependency が実 root から呼べる production adapter（trusted profile resolver、durable terminal store、PTY spawner、agent resolver）を提供していることを確認する。欠ける adapter はその dependency の follow-up として切り出し、integration 側で fake を production path に埋め込まない。

## スコープ

- daemon composition root に lifecycle、agent、generic terminal の single-owner runtime を一度ずつ組み立て、全 IPC connection から同じ owner へ直列化する。
- session create completion の stable `WorkspaceId` / `SessionId` / `WorktreeId` を scope resolver で検証し、agent と terminal launch はその scope だけを受理する。
- agent launch の accepted/progress/final を operation ID と completion fence のまま TUI projection と terminal attachment に配線する。terminal output、exit、disconnect は completion を偽造しない。
- production terminal profile resolver、durable terminal store、PTY worker を root に接続する。client supplied command、argv、cwd、env、secret を wire／record／log に追加しない。
- daemon process を起動する black-box regression test を追加し、独立 data dir と fixture executable だけを使って次のフローを確認する。

## 受け入れ条件

| 操作 | 観測する結果 |
|---|---|
| `usagi` が daemon を起動し session create を送る | response/reconnect 後の snapshot が同じ stable session identity と available worktree を返す |
| その session から agent launch を送る | request の operation ID が accepted と final で同じで、成功 completion は一致する fenced `TerminalRef` を返す |
| agent terminal を attach する | fixture agent の marker output が daemon IPC で読め、client disconnect は process を止めず、reattach で同じ stream を再開できる |
| 同じ session scope から generic terminal launch を送る | trusted fixture profile だけが起動し、input marker と exit completion が terminal ref／subscription／sequence を検証して届く |
| stale / deleting / failed session、stale generation、completion replay | process spawn・worktree create・tab attach を追加で起こさず typed safe error になる |

E2E は実 Codex/Claude、network、credential、利用者の shell 設定に依存しない。agent と shell は test-only trusted fixture executable を使い、起動回数と marker output を assertion する。

## 対象外

- Codex/Claude の実 credential を使う製品 E2E、profile discovery、remote execution。
- daemon crash 後に PTY FD を継続する broker／FD handoff。
- TUI の見た目や command grammar の再設計。
- session lifecycle、terminal coordinator、agent adapter の既存 domain/usecase 契約を置換すること。

## 現時点の blocker

現行 root には session runtime 以外の production owner が接続されていない。特に generic terminal の production resolver/store/spawner と agent request を durable runtime へ渡す root adapter が必要であり、これらを確認せずに E2E を足すと echo dispatch を通るだけの誤った成功テストになる。
