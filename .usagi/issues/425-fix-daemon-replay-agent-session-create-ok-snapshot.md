---
number: 425
title: fix(daemon): replay（冪等）意味論を面間で統一する（agent は失敗を再送、session は失敗 create に Ok+snapshot）
status: todo
priority: medium
labels: [fix, daemon, review]
dependson: []
related: []
created_at: 2026-07-20T11:58:13.882145+00:00
updated_at: 2026-07-20T11:58:13.882145+00:00
---

## 背景

v2 全体の 7 サブシステム並列コードレビュー（2026-07-20）由来。file:line は 2f4dc5b6 時点で検証済み。

## 根拠（検証済み）

- agent 面: `crates/daemon/src/usecase/agent_ipc.rs:299-306` — 同一 `operation_id` の再送に対し `if let Some(existing) = self.operations.get(operation_id) { … return existing.outcome.clone(); }`。`outcome` は `Result` なので**失敗した launch は失敗のまま再送に返る**（失敗が ledger に固定される）。
- session 面: `crates/daemon/src/usecase/session_runtime.rs:298-310` — 同一 operation の再送に対し、当初の成否にかかわらず `Ok(SessionReply { … body: self.snapshot()? })` を返す。**失敗した create の再送が Ok+snapshot になる**。

## 問題

同じ「durable operation ID による冪等応答」の契約が面ごとに逆向き。クライアント（MCP/TUI）はどちらの意味論を前提にすべきか判断できず、session 面では「create は失敗したのに再送で成功に見える」誤認が起きる。

## 改善案（要検討）

- 「replay は当初 outcome を忠実に返す」（agent 面の意味論）へ統一する。
- session 面の ledger に失敗 outcome を保存し、再送には同じエラーを返す。
- 関連: Coordinator 統合 issue（統合すれば意味論も 1 実装に収束する）。

## 受け入れ条件

- [ ] 両面で「失敗した operation の再送は同じ失敗を返す」ことがテストで固定されている。
- [ ] 冪等契約が doc（07-mcp または architecture）に明記されている。
- [ ] coverage 100% を維持する。
