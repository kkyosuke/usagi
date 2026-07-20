---
number: 383
title: fix(daemon): Codex MCP に runtime-fenced caller provenance を注入する
status: todo
priority: high
labels: [daemon, mcp, codex, security]
dependson: []
related: [378, 379, 364]
created_at: 2026-07-20T01:05:43.934926+00:00
updated_at: 2026-07-20T01:05:43.934926+00:00
---

## 背景 / 不具合

Codex を daemon-managed agent として起動すると、`RootCodexProvisioner` は `usagi mcp` を
`mcp_servers.usagi` として注入する。しかし子 MCP process には、それを起動した daemon-owned
Agent runtime の identity / fence / dispatch binding が渡らない。`user_decision_request` の
`dispatch_user_decision` は代替として dispatch store 内の **workspace 全体で唯一の**
`Running` run を推測するため、通常の Codex 起動 session（dispatch run ではない）では
`ownership unknown: decision caller provenance is unknown/ambiguous` となる。

この推測を緩めて「最初の run」や client supplied session/path/agent ID を採用してはならない。
それは root / session / nested session の境界、同時 agent、reconnect、stale/recreated session で
別 caller の decision 作成権限を与える。

## 設計 / やること

### 1. daemon-minted MCP caller context

- `AgentRuntime` が launch admission の成功後に、runtime の `AgentRuntimeRef`（workspace、session
  optional scope、worktree、daemon generation、terminal incarnation）へ結び付く、推測不能で
  process-local な MCP caller credential を発行・保持する。credential 自体、raw argv、秘密値は
  durable launch snapshot、TUI event、log、error へ出さない。
- credential は one Agent runtime / one MCP child 用である。Codex provisioner は daemon が生成した
  context を `usagi mcp` の private spawn provision にだけ渡し、Codex の設定／argv の product-specific
  rendering に閉じ込める。worktree 内の設定ファイル、client supplied path、agent supplied identity を
  authority source にしない。
- core の MCP-to-daemon request に、payload owner とは別の `McpRequestContext`（opaque credential と
  fenced runtime reference）を追加する。CLI MCP server は起動時に provision された context だけを
  forward する。context の field を受け取っても daemon は値を信頼せず、credential と現在の
  daemon-owned runtime registry を照合して identity を再構成する。

### 2. authorization と decision ownership

- daemon は context が active の runtime record、current daemon generation、terminal/runtime
  incarnation、authoritative workspace/session scope、かつ必要な dispatch binding/run に一致するときだけ
  trusted caller provenance を作る。root scope は `session_id: None` として明示的に扱い、managed session
  と同じ workspace 内でも混同しない。
- `user_decision_request` は validated context の caller / worker / run から owner を復元する。
  dispatch-owned worker は対応する `DispatchBinding` を必須にし、通常の interactive agent はその
  runtime 自身の daemon-owned caller record から解決する。全 workspace の Running run を走査する
  fallback を削除する。
- get/list/resolve/cancel/expire も同じ authorization boundary を通す。decision の owner と request
  context の workspace、scope、caller/run fence が一致しない場合は読み書きしない。unknown、forged、
  expired、replayed、generation/terminal mismatch は `ownership_unknown`（または安全な typed error）で
  fail-closed にする。
- reconnect は同じ live runtime に対してのみ context を再利用／再検証できる。daemon restart、agent
  exit、session remove、worktree/session recreation、terminal incarnation replacement は旧 context を
  revoke する。resume/relaunch は新 credential を発行し、旧 credential では authorize しない。

### 3. adapter / composition / documentation

- Codex adapter / root provisioner の MCP injection を runtime-scoped context 付きへ更新する。Claude
  など他 adapter は挙動を変更せず、後続で同じ typed port を opt-in できるようにする。
- MCP serve の direct/manual invocation に context が無い場合、decision mutation は fail-closed とする。
  既存の issue/memory 等、caller ownership を要しない tool の権限を拡大・縮小しない。
- `document/05-daemon.md` と `document/07-mcp.md` の正本に、daemon-minted context、root/session
  scope、reconnect/revocation、fail-closed の責務を記載する。提案 document の triage は本 issue を
  実装追跡先として参照する。

## 受け入れ条件

- daemon が起動した Codex の root scope、managed session、nested session の各 MCP
  `user_decision_request` は、対応する trusted owner で durable decision を一度だけ作成する。
- 同時に動く複数 caller では、各 request が自分の runtime/binding にのみ帰属し、global Running-run
  推測に依存しない。
- MCP child reconnect は live runtime のみ継続できる。daemon restart、agent exit、stale terminal、
  session remove/recreate、同名 session の再作成後、旧 credential/context は拒否され、新 runtime のみ
  新 context で成功する。
- context 無しの手動 MCP、forged credential、改竄した runtime/session/path、foreign workspace、
  mismatched caller/run、unknown binding はすべて fail-closed で、decision/inbox/durable state を変更しない。
- Codex 以外の agent と任意の MCP caller に decision 権限を新たに与えない。client supplied identity/path
  は authorization の入力として採用しない。

## テスト方針

- core: request context の serde/validation、scope/fence/revocation reducer を table-driven で検証する。
- cli/MCP: provisioned context の forwarding、context 無し／改竄値の拒否、tool payload に owner が無いことを
  固定する。
- daemon: fake Codex provisioner + runtime registry で root/session/nested、同時 caller、dispatch worker
  binding、reconnect、stale/recreated session、generation/terminal mismatch を統合検証する。
- composition regression: injected Codex MCP child → MCP serve → IPC → daemon decision store の成功経路と、
  unknown/forged caller が durable mutation を起こさない経路を固定する。実 Codex credential は前提にしない。
- `cargo fmt --all --check`、変更 crate の test、`cargo check --workspace --all-targets`、commit 前の
  `cargo clippy --workspace --all-targets -- -D warnings`。最終 full test / coverage 100% と Markdown link
  check は PR CI で確認する。

## 非目標

- user decision の TUI 表示・回答 UX（#379）。
- supervisor の自動 resume や、任意 MCP caller へ identity を渡す一般-purpose delegation。
- client supplied cwd、session name、path、argv から caller ownership を復元する互換 fallback。
