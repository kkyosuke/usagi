# 11. workspace open 時の pane 復元（live Agent/Terminal の再投影）

> [設計提案一覧](README.md) ｜ [ドキュメント目次](../README.md) ｜ ← 前へ [workspace-root scope](10-workspace-root-scope.md)

workspace を開き直したときに、その workspace/session/root scope に属する **生存中の** daemon-owned
Agent / Terminal runtime を、stable identity で pane tab に復元する設計。実装契約が確定したら
[3. TUI](../03-tui.md)・[4. daemon IPC](../04-ipc.md)・[5. daemon](../05-daemon.md) へ畳み込む。
実装タスクは issue #390（epic）/ #386（daemon）/ #388（tui）で追跡する。

## 目次

- [前提と目標](#前提と目標)
- [非目標](#非目標)
- [現状のギャップ](#現状のギャップ)
- [設計](#設計)
- [誤復元・二重 tab を防ぐ不変条件](#誤復元二重-tab-を防ぐ不変条件)
- [failure matrix](#failure-matrix)
- [実装分割](#実装分割)
- [test strategy](#test-strategy)

## 前提と目標

daemon が terminal / Agent runtime の権威 owner である（[5. daemon](../05-daemon.md#terminal-ownership)）。PTY master・
output journal・process ownership は client disconnect では解放されず、TUI を閉じても runtime は daemon 内で
継続する。したがって同じ daemon が生きている限り、閉じた workspace の Agent / Terminal は **live のまま**残り、
再 attach 可能である。

目標は、workspace を開き直した（同一 client の再 open、または 2 つ目の client の open）ときに、その
workspace の **root scope** と各 **available session scope** に属する live runtime を発見し、pane tab として
stable identity で復元して、ユーザーを直前の Agent / Terminal 作業文脈へ戻すことである。復元は best-effort
かつ fail-closed：証明できない継続性は復元しない。

## 非目標

- **daemon restart / crash / macOS 再起動後の PTY master 復元**。死んだ daemon の PTY master は復元不能で、
  PID だけでは child の所有権を証明できない（[5. daemon](../05-daemon.md#generation-と-orphan-safety)）。この経路は
  [07-pty-crash-continuation](07-pty-crash-continuation.md) の将来設計と、#350 の interrupted 可視化 + 明示
  `ResumeAgent`（provider resume 意味論による**新規** runtime。crash 前 PTY の再 attach ではない）に委ねる。
  本設計は「復元不能な runtime を live tab として復元しない・推測 attach しない・再 spawn しない」ことだけを保証する。
- **open pane layout の TUI-local 永続化**。復元の source of truth は daemon の live inventory とする。
  TUI-local resume state は表示・選択の復元候補に留め、terminal / PTY / session mutation の所有権は daemon に残す
  ([3. TUI](../03-tui.md) の resume data compatibility 契約を維持)。これにより、restore_panes 設定の有無・別 client
  が起動した pane・TUI-local state の欠落に依らず、daemon が live で持つ runtime を一貫して復元できる。
- provider transcript の読み取り・保存。usagi は provider-native id / 表示情報だけを扱う。

## 現状のギャップ

基盤（durable store・stable identity・fencing・reconnect プリミティブ）はほぼ揃っており、欠けているのは
「open 時に daemon inventory を pane tab へ投影する段階」だけである。

| 層 | 既にある | 欠けている |
|---|---|---|
| daemon | generic terminal の `inventory`（`TerminalInventory { terminal, live }`）、fenced `attach` / `resume` / `resync`、`agents.json` / `terminals.json` の durable store | `inventory` が **Agent runtime terminal を列挙しない**（`SharedTerminalOwner` が `Inventory` を `NotOwned` にし generic 側だけが応答）。item に kind（agent/terminal）が無い |
| core | `TerminalRef` / `AgentRuntimeRef` の完全 scope 化と `fences`、`Target`、IPC `TerminalAction::{Inventory,Attach,Resume,Resync}` | scope-filtered な unified inventory item 型 |
| tui | `PaneRuntime::reconnect`（inventory 検証 + 選択 tab 再 attach）、reducer `PaneEvent::Restore` / `PaneState::with_live`、session-scoped `PaneRegistry`（#282）、tab strip（#256） | open flow（`drive_workspace_controller`）が起動時に `inventory()` を呼ばず、空の registry から始まる。`PaneEvent::Restore` に production caller が無い。`reconnect` は既存メモリ tab を検証するだけで **inventory から tab を新規生成しない** |

## 設計

```text
open workspace
  │
  ├─ FsWorkspaceLoader::open ─► lifecycle snapshot（session 行の復元。既存）
  │
  ├─ 初回 frame を paint（#193 の first-paint 契約）
  │
  └─ restore job（off-thread、scope ごと）
        for scope in { root } ∪ { available sessions }:
            daemon.inventory(scope)              # ← #386: agent+terminal, scope-filtered
              │
              └─ items: [{ terminal: TerminalRef, kind, live, agent_display? }]
                    │
                    └─ PaneEvent::Restore(items)  # ← #388: seed tabs
                          │
                          ├─ live && scope 一致 && 未登録(fences) ─► PaneTab::Live を seed
                          ├─ 選択(foreground) tab ─► PaneRuntime::reconnect で attach/resync
                          └─ それ以外 ─► live-but-detached / skip（下表参照）
```

### daemon: unified scope inventory（#386）

- `Inventory` を shared owner 経由で agent owner と generic owner の両方へ fan-out し、結果を merge する。
- 要求 scope（`WorkspaceId` + `Option<SessionId>`（None=root）+ `WorktreeId`）に**完全一致**する runtime だけを返す。
  root scope の解決は #365 の契約（`session_id: None` → trusted repository root、daemon 公開の root worktree id 照合）に従う。
- inventory item は完全な `TerminalRef`（fencing 用）、kind（`agent` / `terminal`）、liveness、agent の場合は Agent tab
  表示に必要な public 情報（public launch plan snapshot 由来）だけを持つ。**argv / environment 値 / secret /
  transcript は含めない**（#253 / #254 の redaction 契約）。
- 現 daemon generation が所有し attach 可能なものだけを `live: true`。`exited` / `ReconcileRequired` /
  `OrphanRunning` / `IdentityUnknown` は attachable として返さない。

### tui: open 時 projection（#388）

- #193 の非同期 launch-job パターンに従い、**初回 frame paint 後**に scope ごとの `inventory` を off-thread で取得し、
  UI thread を daemon handshake で直列ブロックしない。
- `PaneEvent::Restore` / `PaneState::with_live` を拡張し、inventory item から `PaneTab::Live` を **seed（新規生成）**
  できるようにする。Agent item は Agent tab、Terminal item は Terminal tab として `target_for_terminal`
  （`session_id?`）で target ごとに投影する。
- 投影した live tab のうち **選択（foreground）tab だけ**を `PaneRuntime::reconnect` 相当で attach / resync し、
  他は live-but-detached にする（per-target visible projection #282 を維持）。attach は必ず `TerminalRef` で
  fenced に行い、名前 / path から terminal を推測しない。

## 誤復元・二重 tab を防ぐ不変条件

| リスク | 判定 | 動作 |
|---|---|---|
| 死んだ process / non-live item | `live == false` | tab を作らない |
| stale / recreated session | `session_id` が現 lifecycle snapshot に無い | scope mismatch として skip |
| scope mismatch | 別 workspace / 別 worktree / 別 session | skip・attach しない |
| daemon generation 不一致 | 同じ `terminal_id` でも `daemon_generation` 違い | stale として skip・attach しない（`fences` 不一致） |
| duplicate snapshot / 重複 item | `TerminalRef::fences` が既存 tab に一致 | dedup、二重 tab を作らない |
| PTY master 復元不能 | orphan / identity_unknown | live tab にしない・再 spawn しない。session 単位は #350 interrupted、pane 単位は既存 orphan 表示契約 |
| daemon 不通 / transport failure | inventory 取得失敗 | safe feedback を表示し local PTY を生成しない。失敗 scope 以外の復元と手動操作は継続 |

`TerminalRef::fences`（`daemon_generation` / `terminal_id` / `workspace_id` / `session_id?` / `worktree_id` の完全一致）
が dedup と stale 判定の唯一の根拠であり、名前・path・PID による fallback は行わない。

## failure matrix

| 失敗点 | 期待動作 |
|---|---|
| 同一 daemon 生存・runtime live | scope inventory から tab を復元し、選択 tab を attach。二重 tab なし |
| 同一 runtime が複数回 inventory に出る | `fences` で dedup、tab は 1 枚に収束 |
| session が open 前に削除・再作成 | 旧 `session_id` の item は現 snapshot に無く skip。新 session は自身の scope で復元 |
| daemon restart（Agent 生存）| #209 rollover。planned restart 後に正しい generation へ再 attach。generation 不一致の古い ref は復元しない |
| daemon crash / macOS 再起動 | runtime は identity_unknown。live tab を作らず、session は #350 interrupted として sidebar に残り明示 Resume 待ち |
| inventory 取得が遅い / タイムアウト | 初回 frame とキー入力はブロックされない（#193）。該当 scope だけ復元失敗として safe feedback |
| 2 つ目の client が同じ workspace を open | 両 client が同じ live runtime を inventory で発見し、それぞれ subscription を張る（detach は当該 connection のみ） |

## 実装分割

| issue | 層 | 内容 |
|---|---|---|
| #390 | epic | 目標・scope・不変条件・受け入れ条件の親 |
| #386 | daemon + core | `terminal inventory` を agent runtime terminal も含む scope-filtered な unified inventory にする |
| #388 | tui | workspace open 時に scope inventory を pane tab へ投影する（#386 に依存） |

## test strategy

- **daemon（#386）**: agent + generic 両 coordinator を持つ fake daemon fixture で、root + 複数 session scope の
  混在 runtime に対する scope filter・kind 付与・live/non-live 分類を検証。inventory item schema の durable
  round-trip と後方互換。古い generation / 別 scope が inventory に混ざらない fence 回帰。secret / argv /
  transcript 非露出の redaction。
- **tui（#388）**: fake inventory port（root + 複数 session、dead、stale-session、scope-mismatch、
  generation-mismatch、duplicate-snapshot、orphan / identity_unknown）で restore projection・dedup・safe 縮退を
  検証（`pane_runtime.rs` / `parity_suite.rs` の既存 fake `TerminalPort` と `resume_compatibility_fixture...` を拡張）。
  first-paint 順序（inventory off-thread でキー入力を待たせない）。no-duplicate-tab の収束。投影後の
  attach / resync / input / resize / detach / exit の runtime regression。
- 両 issue とも coverage 100% を維持する。
