# 11. workspace open 時の pane 復元（live Agent/Terminal の再投影）

> [設計提案一覧](README.md) ｜ [ドキュメント目次](../README.md) ｜ ← 前へ [workspace-root scope](10-workspace-root-scope.md)

workspace を開き直したときに、その workspace/session/root scope に属する **生存中の** daemon-owned
Agent / Terminal runtime を、stable identity と durable Agent display intent で pane tab に復元する設計。
現在の実装契約の正本は [3. TUI](../03-tui.md)・[4. daemon IPC](../04-ipc.md)・
[5. daemon](../05-daemon.md) である。基盤は issue #390（epic）/ #386（daemon）/ #388（tui）、
Agent intent reconciliation は #506 で追跡する。

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
- **runtime state の TUI-local 永続化**。TUI は liveness、PTY ownership、provider conversation、output、generic Terminal
  本文を保存しない。一方、Agent tab の順序・選択・continuation-scoped dismissal は user-local / workspace-scoped な
  `AgentTabIntent` として永続化する。daemon inventory が runtime の正本、local intent が表示 intent の正本であり、
  open / reconnect 時に両者を照合する。別 client が起動した inventory-only runtime は決定的に末尾へ追加する。
- provider transcript の読み取り・保存。usagi は provider-native ID を local intent に含めず、
  provider-neutral な `AgentContinuationRef` と safe label だけを扱う。

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
  ├─ local AgentTabIntent を load（欠落/未知 schema は空 intent）
  │
  └─ restore job（専用 daemon connection、off-thread）
        terminal_inventory + agent_inventory
              │
              └─ reconcile(saved order/selection/dismissal, live+durable history)
                    │
                    └─ PaneEvent::RestoreBatch(targets, dispatch fences)
                          │
                          ├─ exact live saved Agent ─► 保存順で seed
                          ├─ inventory-only live runtime ─► 決定的に末尾へ append
                          ├─ selected foreground ─► attach/resync
                          └─ background / unselected ─► detached のまま保持
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

### tui: open 時 projection と display intent（#388 / #506）

- #193 の非同期 launch-job パターンに従い、**初回 frame paint 後**に scope ごとの `inventory` を off-thread で取得し、
  UI thread を daemon handshake で直列ブロックしない。
- versioned `AgentTabIntent` は workspace ID、root / managed-session target、完全な last-known `TerminalRef`、
  `AgentContinuationRef`、tab 順序・選択・dismissal だけを atomic file に保存する。provider ID、argv、environment、
  transcript、terminal output は含めない。file lock と revision CAS で複数 client の stable-key mutation を直列化し、
  close intent は authoritative GC または明示 reopen まで union する。
- `PaneEvent::RestoreBatch` は target ごとの saved Agent 順を先に、inventory-only Agent と generic Terminal を後ろへ
  決定的に投影する。exact `TerminalRef` で dedup し、同じ continuation の replacement incarnation は同じ slot へ収束する。
  resumable だが non-live の slot は intent に保持し、provider resume や replacement spawn は開始しない。
- 投影した live tab のうち **選択（foreground）tab だけ**を `PaneRuntime::reconnect` 相当で attach / resync し、
  他は live-but-detached にする（per-target visible projection #282 を維持）。attach は必ず `TerminalRef` で
  fenced に行い、名前 / path から terminal を推測しない。
- restore result は dispatch 時の UI interaction count / registry revision を持つ。一致する結果だけが order / selection を
  置換でき、遅延結果は新しい ref の append に限る。inventory transport failure は bounded backoff で再試行し、last valid
  intent を空 snapshot で上書きしない。

## 誤復元・二重 tab を防ぐ不変条件

| リスク | 判定 | 動作 |
|---|---|---|
| 死んだ process / non-live item | `live == false` | tab を作らない |
| stale / recreated session | `session_id` が現 lifecycle snapshot に無い | scope mismatch として skip |
| scope mismatch | 別 workspace / 別 worktree / 別 session | skip・attach しない |
| saved / current generation 不一致 | saved exact ref の trusted active / draining owner を両 inventory で照合 | exact owner が live なら保持し、active / draining の双方に owner が無いと daemon が確定した場合だけ stale |
| duplicate snapshot / 重複 item | `TerminalRef::fences` が既存 tab に一致 | dedup、二重 tab を作らない |
| dismissed continuation の replacement / interrupted record | durable lineage が一致 | 明示 reopen まで tab を抑止し、runtime は停止しない |
| partial / failed inventory | complete durable absence を証明しない | slot / dismissal を GC せず retry |
| delayed restore response | interaction / registry revision が進んだ | append のみ許し、order / selection / close を上書きしない |
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
| 2 つ目の client が同じ workspace を open | lock + CAS merge で intent を更新し、両 client とも選択 foreground だけ subscription を張る（detach は当該 connection のみ） |

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
