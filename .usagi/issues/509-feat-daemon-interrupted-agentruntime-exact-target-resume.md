---
number: 509
title: feat(daemon): interrupted AgentRuntime を exact target で resume 可能にする
status: in-progress
priority: high
labels: [review, v2, daemon, agent, recovery]
dependson: [503]
related: [214, 350, 363, 504]
parent: 505
created_at: 2026-07-21T21:20:52.038648+00:00
updated_at: 2026-07-21T22:16:36.690585+00:00
---

## 問題・影響

#503 で `ProviderResumeRef` は runtime ごとに durable になったが、公開 `ResumeAgent` は managed `SessionId` を受ける session-scoped 操作である。[agent IPC](../../crates/daemon/src/usecase/agent_ipc.rs) は resume source 候補の provider identity が一意であることを要求するため、同じ session に異なる Claude / Codex 会話や過去 runtime が複数あると ambiguous になり、利用者がどの tab を再開するか指定できない。

workspace root Agent は `session_id: None` で launch / inventory できる一方、resume action は `SessionId` 必須である。これにより root tab は provider metadata があっても復帰できない。

## 対象責務

provider conversation lineage ごとに secret-free な daemon-issued `AgentContinuationRef` を追加する。これは provider-native ID と異なる opaque public key で、live runtime inventory、interrupted / resumable inventory、resume 成功後の replacement runtime に共通して現れる。各 runtime incarnation は別に fence し、resume final は source → replacement の明示 relation を返す。これにより TUI は provider ID・名前・path・PID を使わず、同じ tab slot を live → interrupted → new live へ対応付けられる。

interrupted runtime ごとに client-safe な `AgentResumeTarget` を projection する。target は `AgentContinuationRef`、opaque source ID、workspace / optional session / worktree / runtime incarnation / adapter revision の fences を持ち、raw provider-native session ID、cwd、argv、environment を client に公開も入力もさせない。

`ResumeAgent` は exact target と operation ID を受け、次を検証してから新しい daemon-owned runtime を予約・spawn する。

- workspace と `session_id: Option<SessionId>`、worktree、runtime incarnation が durable record と完全一致する。
- source runtime が non-live / interrupted で、対応する `ProviderResumeRef` が valid、adapter-compatible、provider-specific capture policy を満たす。
- 同じ target の live replacement または in-flight resume がなく、capacity / operation fence を取得できる。
- 成功時は選択 source だけを supersede し、新しい fully fenced `TerminalRef` と同じ `AgentContinuationRef`、source → replacement relation を返す。他の履歴は変更しない。

resumable inventory は root と managed session、同一 scope の複数 history を別 item として返し、resume available / unavailable と provider ID を含まない safe reason を持つ。並び順は durable timestamp + stable ID 等で deterministic にする。

CLI / TUI / MCP は同じ exact-target wire contract の薄い client とし、provider ID や cwd を組み立てない。legacy session-scoped request を互換提供する場合、daemon が eligible target を厳密に 1 件だけ解決できる時に限り exact request へ変換し、0 件 / 複数件は typed failure にする。「最新」や provider 種別からの暗黙選択は禁止する。

Claude は daemon-generated UUID を exact target の内部 metadata として使用する。Codex は [#504](./504-feat-daemon-codex-structured-capture-wiring.md) の正式 structured capture がある runtime だけ available とし、未実装 / capture failure では unavailable を返す。`--last`、transcript / state DB の探索、session-wide identity inference へ fallback しない。

## 受入条件

- [ ] root と managed session の interrupted runtime を同じ API で列挙し、同一 scope の複数 Claude / Codex history を stable な別 target として返す。
- [ ] live / resumable inventory と replacement final が同じ secret-free `AgentContinuationRef` を返し、runtime incarnation と source relation で stale / replay を fence する。
- [ ] `AgentContinuationRef` / opaque target は durable reload と daemon restart を越えて同じ lineage に安定し、新しい conversation lineage / runtime incarnation へ再利用されない。
- [ ] exact target の resume は選択した provider conversation だけを新 runtime に一度だけ起動し、新しい `TerminalRef` を返す。
- [ ] duplicate operation / double click / reconnect replay は同じ final へ収束し、capacity leak・二重 spawn・別 history の supersede を起こさない。
- [ ] stale target、scope / incarnation / worktree / adapter mismatch、live source、metadata 欠落、provider unavailable、ambiguous legacy record を typed failure にし、空会話や推測 resume を行わない。
- [ ] client payload、snapshot、error、log に raw provider ID、argv、environment、transcript を露出しない。
- [ ] old durable record は resume unavailable として互換に読み、daemon 起動を失敗させない。

## 必須回帰テスト

durable round-trip / migration と IPC fixture で、restart を越える continuation stability / non-reuse、source → replacement relation、root、複数 session、同一 session の同一 provider 複数履歴、Claude / Codex 混在、exited / reclaimed / identity_unknown、duplicate operation、restart replay を検証する。CLI / TUI / MCP exact request と legacy 0 / 1 / multiple target compatibility も同じ fixture で固定する。

Claude fixture は exact UUID が `claude --resume <id>` に渡ることを確認する。Codex は既存 structured-capture boundary へ注入した正式 ID だけが `codex resume <id>` に渡る adapter / IPC fixture と、capture 無しでは unavailable のまま provider file を読まない fixture を固定する。shipping provider channel の供給と product E2E は #504 / #510 が所有する。

## docs / migration

[IPC](../../document/04-ipc.md) と [daemon](../../document/05-daemon.md) に `AgentResumeTarget`、optional session scope、redaction、idempotence、supersede transition を記載する。旧 session-scoped resume request の互換期間と safe rejection / migration を定め、複数 history を暗黙選択しない。
