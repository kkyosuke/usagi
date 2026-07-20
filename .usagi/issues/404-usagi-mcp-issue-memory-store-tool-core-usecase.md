---
number: 404
title: usagi mcp: issue/memory の store 系 tool を core usecase に接続する
status: in-progress
priority: medium
labels: [mcp]
dependson: [401]
related: []
parent: 400
created_at: 2026-07-20T04:54:15.169914+00:00
updated_at: 2026-07-20T07:09:35.751124+00:00
---

親: #400。依存: #401。issue/memory の store 系 tool を、cwd の `.usagi/` を読み書きする core usecase に接続する（CLI コマンドと同じ usecase を呼ぶ兄弟実装）。

## 対象 tool（10）

- issue（6）: `issue_create` / `issue_get` / `issue_to_prompt` / `issue_search` / `issue_update` / `issue_delete`
- memory（4）: `memory_save` / `memory_get` / `memory_search` / `memory_delete`

## 現状の断絶（根拠）

`crates/cli/src/mcp/tools/issue.rs` / `memory.rs` の各 `Tool` は `name`/`description`/`input_schema` のみ実装し、`call` は既定スタブ（`crates/cli/src/mcp/tool.rs:33-35`）→ 実測でも `-32603 tool not yet implemented`。docs（`document/07-mcp.md:54-55`）は「store 系として core usecase で直接読み書き」と先行記載しており不一致。

## 完了条件

- [ ] 10 tool の `call` を実装し、CLI と同じ core usecase（issue store / memory store）を呼んで cwd の `.usagi/issues/`・`.usagi/memory/` に対する実 durable 効果と結果 JSON を返す。
- [ ] root/コーディネータ制約の維持: workflow 規約どおり、`main`（`.usagi/sessions/` 配下でない）チェックアウトからの issue 書き込み系（create/update/delete）は拒否する現行ポリシーを保つ（session worktree からのみ許可）。既存の root 拒否挙動を回帰させない。
- [ ] `presentation → usecase → domain` の依存方向を守る（tool は presentation に徹し独自ロジックを持たない）。
- [ ] **production E2E**: `usagi mcp` から `issue_create`→ファイル生成→`issue_get`/`issue_search` で読み戻し、`memory_save`→`memory_get` を stdio→durable で固定。
- [ ] `document/07-mcp.md` の store 系記述を実挙動に一致。coverage 100%。
