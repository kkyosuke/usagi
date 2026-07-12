---
number: 250
title: feat(core): AgentProfile / LaunchPlan / capability を導入する
status: done
priority: high
labels: [core, agent, design]
dependson: []
related: [142, 145, 146, 219, 248]
created_at: 2026-07-12T22:26:31.638453+00:00
updated_at: 2026-07-12T22:26:52.369058+00:00
---

## 目的

v2 daemon が Agent runtime を起動する前に、agent 製品・CLI・PTY・IPC に依存しない `usagi-core` の契約として `AgentProfile`、capability、`LaunchRequest`、`LaunchPlan` を導入する。core は「何を要求し、何を満たせるか」と不変の起動意図だけを表し、Claude/Codex の CLI 文法・設定形式・hook 実装は adapter 側に閉じる。

## 背景

現行 v2 は `AgentRuntimeId`、session lifecycle、terminal registry、daemon の typed launch resolution を備える一方、agent 起動の profile と plan の core 契約をまだ持たない。旧 v1 の #139 は `src/domain/agent.rs` に capability / launch plan を導入済みだが、v2 `usagi-core` へは移植されておらず、daemon の `AgentRuntimeId` と結び付く durable boundary も定義していない。

## 設計契約

| 語彙 | core の責務 | 所有者・永続化 | adapter への境界 |
| --- | --- | --- | --- |
| `AgentProfile` | stable profile ID、利用者に選べる表示名、capability descriptor、許可される launch mode / request option を表す静的 descriptor | profile registry は code-defined であり永続化しない。選択された profile ID は session の immutable launch intent に snapshot する | adapter は profile ID を受け、対応できるかを宣言する。program 名・CLI flag・設定ファイル名を profile に持ち込まない |
| `AgentCapability` | `resume`、initial prompt、headless、phase reporting、MCP wiring 等の product-neutral な能力を closed vocabulary で表す | profile から導出する値であり、runtime token・IPC handshake capability・`LifecycleCapabilities` とは別物で永続化しない | adapter は capability を満たせない request を typed error で拒否する。adapter が独自の flag を core enum に追加しない |
| `LaunchRequest` | profile ID、mode、model selector、resume、initial/headless prompt、worktree/session scope、必要 capability を持つ意図 | daemon が terminal/runtime reservation 前に immutable launch intent として durable operation に保存する。secret、render 済み argv、raw hook payload は保存しない | adapter は request を受けて plan を生成する。request に Claude/Codex 固有 config を加えない |
| `LaunchPlan` | process 起動に必要な shell-neutral `program` / `argv`、非 secret env allowlist、working directory と、request/profile への provenance を表す値 | daemon が execution に必要な最小 immutable snapshot を operation/terminal record に保存するかを本 issue で明文化する。再解決で意味が変わる値は durable にする | adapter が唯一の renderer / resolver。core は shell escaping、TOML/JSON、hook install、subprocess spawn を実装しない |

### 型境界と不変条件

- `AgentProfileId` は stable な typed ID とし、表示名や executable 名を identity にしない。profile selection と `AgentRuntimeId`（一回の起動）を混同しない。
- capability は agent product capability だけを表す。IPC negotiation capability、runtime phase-report token、terminal/lifecycle authorization capability は既存のそれぞれの所有者に残す。
- `LaunchPlan` は argv を保持し shell command string を正本にしない。実際の process spawn は daemon infrastructure の責務であり、core domain は `std::process`・PTY・IO に依存しない。
- model は opaque validated selector として request に置き、adapter ごとの model allowlist / flag spelling は adapter 内に置く。
- request を profile capability と照合する pure validation は core usecase/domain に置く。installed executable の検査、設定ファイル materialization、MCP/hook provisioning、env secret 注入、process spawn は adapter / daemon infrastructure に置く。
- profile registry は durable daemon state の正本ではない。operation に記録するのは profile ID、validated request、必要なら execution-reproducible plan snapshot / schema revision であり、adapter private config や secret は記録しない。daemon restart / replay では snapshot と revision 不一致を typed failure にし、最新 registry から黙って別の意味へ再解決しない。

### 所有者

```text
core domain/usecase
  AgentProfile / AgentCapability / LaunchRequest / LaunchPlan
  profile-request validation / typed rejection
          ▲                         │
          │ consumes                │ returns
adapter (Claude or Codex; separate issues) ── profile-specific plan renderer
          │                                  │
          └──────── daemon usecase ──────────┘
             resolve once, durable snapshot, terminal/runtime reservation
                         │
                    daemon infrastructure
             provision config / inject secret / spawn PTY
```

## スコープ

- `usagi-core::domain` に上記 value type、closed capability vocabulary、typed validation error を置く。
- `usagi-core::usecase` に profile lookup と request validation の pure port / contract を置く。profile catalog の実装位置と adapter registration seam を確定する。
- daemon が消費する immutable launch snapshot、operation/terminal/runtime record との参照関係、schema/version mismatch の failure policy を定義する。
- profile capability と request validation の unit test、plan の secrecy / replay / serialization boundary を固定する。

## 非対象

- Claude / Codex の command renderer、MCP / hook / config provisioning、実 executable 検出、PTY spawn の実装。
- CLI 固有 model allowlist、flag spelling、JSON / TOML / shell escaping の core への移植。
- IPC wire、runtime token、lifecycle capability、terminal authorization の再設計。
- profile をユーザー設定・plugin discovery・network registry として永続化すること。

## 依存関係と後続ロードマップ

| 段 | issue / 担当 | この issue との関係 |
| --- | --- | --- |
| #1 | v2 typed ID / lifecycle / terminal foundation（#214 / #217 / #218） | 完了済み。`AgentRuntimeId`・session/terminal scope を再定義せず利用するため blocker にはしない |
| #2 | 本 issue | core 契約・durability boundary を追加する。adapter 実装を含めない |
| #3 | Claude adapter | #2 の public profile/request/plan contract だけを消費して Claude 固有 renderer / provisioner を実装する。Codex と write-set を分離できる |
| #4 | Codex adapter | #2 の同じ contract を消費して Codex 固有 renderer / provisioner を実装する。Claude 固有型を参照しないため #3 と並行可能 |
| #5 | daemon launch resolver / orchestration validation | #2 の validated request と durable plan snapshot を terminal/runtime reservation、queue/autostart、session override validation へ接続する。daemon は adapter を直接識別せず profile/plan port を消費する |

既存 #142 / #145 / #146 は旧 v1 adapter work として related であり、v2 移植の blocker ではない。#219 は daemon control contract、#248 は daemon terminal event の先行設計として related だが、本 issue はいずれの IPC/terminal wire も変更しない。

## 受け入れ条件

- `usagi-core` の public contract が `AgentProfile`、capability、`LaunchRequest`、`LaunchPlan`、typed validation error を持ち、daemon / Claude / Codex crate を参照しない。
- profile ID、runtime ID、IPC capability、authorization token、lifecycle capability が型・文書・test で区別される。
- profile の static metadata と durable launch snapshot の保存対象 / 非保存対象が明文化され、secret / adapter private config / rendered shell string を durable state に入れない。
- incompatible profile/request、unsupported capability、unknown profile、adapter revision mismatch、replay/restart を fail-closed な typed result にする。
- Claude と Codex の adapter issue が core を変更せず別 crate / adapter module で並行実装でき、双方が同じ request/plan contract を消費する test double で示される。
- existing terminal/lifecycle/IPC の capability 語彙を変更せず、名称衝突を避ける。

## テスト方針

- core domain: profile catalog の全件性、capability matrix、unknown/unsupported request、typed ID separation、argv normalization の table-driven test。
- core usecase: validation、profile revision mismatch、durable snapshot の replay / stale rejection、secret-redaction serialization test。
- adapter contract: fake Claude / fake Codex renderer が同じ valid request を独立に plan 化し、unsupported capability を同じ typed error category にする contract test。実 CLI は起動しない。
- daemon integration は #5 で fake resolver + fake PTY により「resolve once → persist snapshot → reserve runtime」の crash/replay を検証する。本 issue では daemon/PTY E2E を追加しない。

## 実装上の注意

`AgentProfile` を CLI 名・flag・config path の enum にしない。Claude/Codex の相違は adapter の renderer / provisioner に閉じ、core が扱うのは選択・能力・意図・再現可能な起動 plan のみとする。
