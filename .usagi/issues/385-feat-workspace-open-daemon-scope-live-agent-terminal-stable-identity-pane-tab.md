---
number: 385
title: feat: workspace open 時に同一 daemon 上の scope 内 live Agent/Terminal を stable identity で pane tab へ復元する
status: todo
priority: high
labels: [tui, daemon, terminal, agent, orchestration, restore, design]
dependson: []
related: [193, 195, 256, 282, 350, 209, 254, 363, 367]
created_at: 2026-07-20T01:41:38.171645+00:00
updated_at: 2026-07-20T01:43:24.119182+00:00
---

## 目的

workspace を開き直したとき（同一 client の再 open、または別 client の open）、その workspace/session/root scope に属する **生存中（同一 daemon generation が所有し attach 可能）** の daemon-owned Agent / Terminal runtime を、stable identity で pane tab に復元する。ユーザーが直前の Agent / Terminal 作業文脈へ戻れるようにする。

## 現状（調査結果）

基盤はほぼ揃っているが、「open 時に inventory を tab へ投影する段階」だけが欠けている。

**既に存在する（fenced 済み）**
- durable store: `agents.json`（`DurableRuntimeRecord`/`AgentRuntimeRef`）・`terminals.json`（`DurableTerminalRecord`/`TerminalRef`）。いずれも `workspace_id` / `session_id?`（None=root）/ `worktree_id` / `daemon_generation` + `ProcessIdentity` で完全に scope 付き。
- stable identity + fencing: `TerminalRef::fences`（incarnation 一致、名前/path/PID fallback なし）、`Target::{Root(WorkspaceId), Session(SessionId)}`、`OperationId`。
- client reconnect プリミティブ: IPC `TerminalAction::{Inventory, Attach, Resume, Resync}`、`PaneRuntime::reconnect`（inventory 検証 + 選択 tab 再 attach + fenced teardown）、reducer の `PaneEvent::Restore` / `PaneState::with_live`。
- session-scoped tab registry（#282）、tab strip（#256）、pending→live lifecycle、session 行の open 時復元（daemon lifecycle projection）。
- PTY master 復元不能時の表示/attach 契約（orphan/identity_unknown → read-only・input 無効・再 spawn しない）と daemon restart reconcile（#350 / #209）。

**欠けている本体（ギャップ）**
1. **daemon**: `terminal inventory` は generic terminal coordinator のみが応答し、**agent runtime terminal を列挙しない**（`SharedTerminalOwner` が Inventory を `NotOwned` にして generic 側へ落とす）。restore 投影に必要な kind（agent/terminal）と liveness も不足。
2. **tui**: workspace open（`drive_workspace_controller`）は空の `PaneRegistry` / 空の `WorkspaceUi.terminals` から始まり、起動時に `port.inventory()` を呼ばない。`PaneEvent::Restore` に production caller が無く、`PaneRuntime::reconnect` は shell に配線されていない。しかも `reconnect` は**既存メモリ tab を検証するだけで inventory から tab を新規生成しない**。

結果として、同一 daemon が runtime を live で所有していても、workspace を開き直すと tab は復元されず「まっさら」で始まる。

## scope（この epic に含める / 含めない）

**含める**
- 同一 daemon generation が所有し attach 可能な live runtime（Agent / Terminal、session pane と root pane の両方）の、open 時 tab 復元。

**含めない（既存契約に委譲）**
- daemon restart / crash / macOS 再起動後の PTY master 復元。これは復元不能であり、#350（interrupted 可視化 + 明示 `ResumeAgent`）・#209 / #221 の explicit orphan 契約に従う。本 epic は「復元不能な runtime を live tab として復元しない・推測 attach しない・再 spawn しない」ことだけを保証する。
- open pane layout の TUI-local 永続化。復元の source of truth は daemon の live inventory とし、TUI-local resume state は表示・選択の復元候補に留める（[03-tui.md](../../document/03-tui.md) の resume data compatibility 契約を維持）。

## 誤復元・二重 tab を作らない不変条件

| リスク | 期待動作 |
|---|---|
| 死んだ process / non-live inventory item | tab を作らない |
| stale / recreated session（`session_id` が現 lifecycle snapshot に無い） | scope mismatch として skip |
| scope mismatch（別 workspace / 別 worktree / 別 session） | skip、attach しない |
| daemon generation 不一致（同じ `terminal_id` でも古い generation） | stale として扱い attach しない |
| duplicate snapshot / 同一 runtime の重複 | `TerminalRef::fences` で dedup、二重 tab を作らない |
| PTY master 復元不能（orphan / identity_unknown） | 既存契約に従い live tab にしない・再 spawn しない |

## 分割

- **#386**（daemon + core）: `terminal inventory` を agent runtime terminal も含む scope-filtered な unified inventory にする。
- **#388**（tui）: workspace open 時に scope inventory を pane tab へ投影する（restore-on-open 配線）。#386 に依存。

## 設計正本

[document/proposals/11-workspace-restore-panes.md](../../document/proposals/11-workspace-restore-panes.md)

## 受け入れ条件（epic）

- 同一 daemon 生存中に workspace を開き直すと、scope 内の live Agent / Terminal が stable identity で tab に復元され、選択 tab が attach される。
- 上表の全リスクケースで誤復元・二重 tab・local spawn が起きない。
- PTY master 復元不能ケースは既存の interrupted / orphan 契約に従い、live tab を作らない。
- durable store / fake daemon / reconnect / runtime regression test で回帰が固定される。
- coverage 100%（子 issue で担保）。
