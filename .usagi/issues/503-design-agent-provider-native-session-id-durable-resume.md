---
number: 503
title: design(agent): provider-native session ID を durable に保持し明示 Resume を実現する
status: done
priority: high
labels: [daemon, tui, cli, mcp, agent, recovery, design]
dependson: []
related: [350, 388, 390]
created_at: 2026-07-21T10:34:02.909659+00:00
updated_at: 2026-07-21T10:35:30.817523+00:00
---

## 背景・調査結果

#350 は provider-native resume metadata と `SessionAction::ResumeAgent` を設計したが、現実装には未反映である。
`DurableRuntimeRecord` は `runtime`、公開 launch snapshot、state、process、operation outcome 等を持つだけで、
provider-native session ID を保持しない。`SessionAction` にも `ResumeAgent` は無い。

現在の `AgentCapability::Resume` は provider 固有の ID を表さない。interactive launch の Codex adapter は
`codex resume --last`、Claude adapter は `claude --continue` を render するだけである。したがって daemon
crash/restart/macOS 再起動後に、利用者が元の provider 会話を特定して安全に再開する durable contract は無い。

provider CLI の正式な入力契約は次のとおりである。

| provider | 新規 interactive launch | 明示 resume |
|---|---|---|
| Claude | usagi が UUID を発行し `claude --session-id <uuid>` を渡す | `claude --resume <uuid>` |
| Codex | provider の正式な構造化経路で session ID を取得できる場合だけ記録する | `codex resume <SESSION_ID>` |

Codex の ID を provider state/transcript ファイルから推測、走査、parse して取得してはならない。正式な構造化
取得経路が無い、または失敗した場合は resume 不可として fail-closed にする。

## 目的

v2 で pane / Agent runtime ごとに provider-native session identity を durable に保持し、daemon crash/restart/macOS
再起動後も、利用者の明示操作だけで同じ provider 会話を新しい daemon-owned PTY/Agent runtime として再開できるようにする。

これは crash 前 PTY への再 attach ではない。`SessionId` / `TerminalRef` と provider-native identity を混同せず、
scope と adapter の検証を通過した新規 runtime だけを spawn する。

## durable model と所有境界

`DurableRuntimeRecord`（またはその pane/agent runtime に一対一で関連する durable record）に、secret-free な
`ProviderResumeRef` を追加する。

| field | 用途 |
|---|---|
| provider | code-defined provider 種別（Claude/Codex） |
| native_session_id_or_name | provider が resume 入力として正式に受け付ける opaque ID/name |
| adapter_revision | resume renderer と保存済み metadata の互換性検証 |
| scope_fences | workspace/session/worktree と runtime incarnation を含む完全 scope |
| last_known_status_or_phase | interrupted projection と操作可否の表示 |
| capture provenance | Claude の usagi-generated ID、または Codex の正式 structured capture 成功を区別 |

- `SessionId` は usagi managed session の identity、`TerminalRef` は daemon-owned PTY の fenced identity、
  `ProviderResumeRef` は provider 会話の opaque identityであり、相互に置換・fallback してはならない。
- durable state に argv、environment 値、secret、credential、transcript 本文、raw CLI output を保存しない。
  provider ID は sensitive metadata とし、IPC/observability では必要最小限の識別不能な表示または redaction 方針を定める。
- metadata は launch reservation より前に durable atomic write し、spawn 成功/失敗、daemon restart reconcile、
  migration のいずれでも partial snapshot を公開しない。

## provider 別 capture / resume 契約

### Claude

- 新規 interactive launch ごとに daemon が UUID を生成し、adapter が `claude --session-id <uuid>` を render する。
- 同じ UUID を `ProviderResumeRef` に保存し、resume は `claude --resume <uuid>` を render する。
- `--continue` は last session という曖昧な provider-global 選択になるため、durable resume path では使わない。

### Codex

- resume は `codex resume <SESSION_ID>` を render する。`--last` は durable resume path では使わない。
- ID capture は provider が公開する正式な構造化 API/event/command result を adapter 境界で受ける場合だけ許可する。
  transcript、state database、設定、履歴ファイルを検出・走査・parse する実装は非目標かつ禁止する。
- launch 時に正式 capture が得られない場合、runtime 自体は通常起動できても `ProviderResumeRef` は保存せず、
  restart 後は resume 不可として interrupted projection に理由を返す。空 ID、推測 ID、`--last` への downgrade はしない。

## 明示 Resume のフロー

```text
利用者の CLI / TUI / MCP 明示操作
  -> daemon IPC SessionAction::ResumeAgent(operation ID, session incarnation)
  -> ProviderResumeRef + scope fences + adapter revision を検証
  -> live runtime の有無と operation fence を検証
  -> 新しい daemon-owned PTY / Agent runtime を予約・durable 化・spawn
  -> 成功 final の新しい TerminalRef だけを pane に attach
```

- TUI 起動、pane/inventory restore、daemon restart、reconnect、launchd 起動からは Resume を自動実行しない。
- interrupted projection は「resume 可/不可」と safe reason を session/pane に表示するが、旧 PTY を live tab として
  復元せず、旧 `TerminalRef` に attach/resume/input/resize しない。
- TUI は request 中に session-scoped pending Agent pane を 1 枚だけ表示し、成功 final の新しい fully fenced
  `TerminalRef` にだけ attach/poll を開始する。名前、path、PID、旧 runtime ID から terminal を推測しない。
- live Agent が同じ scope にある場合は Resume を拒否する。同じ operation の再送、double click、reconnect は
  operation ID と session incarnation で収束させ、二重 spawn/二重 pane を禁止する。
- metadata 欠落、provider binary 不在、Codex capture 不可、adapter revision 不一致、scope/worktree 不一致、
  orphan/stale identity、transport failure は typed safe failure とし、local spawn・空会話開始・旧 PTY attach をしない。

## migration と互換性

- 既存 `agents.json` / `DurableRuntimeRecord` に `ProviderResumeRef` がないことは互換な「resume 不可」を意味する。
  deserialize/startup を失敗させず、interrupted と safe reason を projection する。
- schema version と durable atomic writer を更新し、未知の将来 field と旧 record の migration 方針を store contract に
  固定する。migration が provider ID を捏造・推測・補完してはならない。
- provider-native ID を持たない通常 launch、headless launch、既存 #390 の同一 daemon live inventory restore は
  既存の契約を維持する。

## 分割可能な実装タスク

1. **core / daemon domain & store**: `ProviderResumeRef`、provider-specific validated opaque ID、durable schema/
   migration/atomic persistence、redaction 型、restart interrupted projection を追加する。
2. **provider adapters**: Claude UUID の生成・`--session-id`/`--resume <id>` rendering、Codex の正式 structured capture
   boundary・`codex resume <id>` rendering・capture 不可時の typed rejectionを実装する。
3. **daemon IPC**: `SessionAction::ResumeAgent`、scope/adapter/live-runtime/operation fence、new PTY spawn、typed final
   result を実装する。
4. **clients**: CLI、TUI の interrupted/pending/live pane state machine、MCP explicit entry を daemon IPC の薄い client
   として追加する。client が provider argv/ID/cwd を指定できないようにする。
5. **docs / tests**: `document/03-tui.md`（UI）、`document/04-ipc.md`（wire）、`document/05-daemon.md`（ownership/
   durable resume）を実装に合わせて更新し、下記の fixture test を追加する。

## 受け入れ条件・テスト方針

- Claude の新規 interactive launch が usagi-generated UUID を `--session-id` で固定し、restart 後の explicit
  resume が同一 ID を `claude --resume` に渡すことを fake adapter で検証する。
- Codex は正式 structured capture に成功した ID だけを保存し `codex resume <id>` を render する。capture 不可時、
  transcript/state file を読まず、`--last` に downgrade せず resume 不可になることを fixture で検証する。
- durable round-trip、旧 record の backward compatibility、secret/argv/env/transcript/raw output の非保存・非露出、
  provider ID redaction を store/IPC/log fixture で固定する。
- daemon crash/restart/reboot 相当で旧 PTY に再 attach せず interrupted になり、valid ref の明示 Resume だけが
  新しい fenced runtime を一度だけ spawn することを fake process/provider で検証する。
- stale scope/worktree、adapter mismatch、live Agent、duplicate operation、duplicate click、reconnect、provider unavailable
  の各ケースで fail-closed・二重 spawnなしを daemon/TUI reducer/integration fixture で検証する。
- full coverage を維持する。
