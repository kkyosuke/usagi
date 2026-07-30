---
number: 582
title: fix(cli): MCP dispatch の runtime allowlist に sakana-ai を追加する
status: todo
priority: high
labels: [cli, mcp, bug, ssot]
dependson: []
related: []
created_at: 2026-07-30T10:46:23.287772+00:00
updated_at: 2026-07-30T10:46:23.287772+00:00
---

## 背景

daemon の `AdapterRegistry`（`crates/daemon/src/usecase/orchestration.rs:69-71`）は `codex` / `sakana` / `claude` の3 profile を登録し、`sakana-ai` は `document/03-tui.md:150-152` で `claude` / `codex` と並ぶ closed vocabulary の一員として文書化されている（Config 画面・Closeup `agent -m sakana.ai` から起動可能）。

一方、MCP dispatch 面（`session_dispatch` / `session_create` 等）の runtime allowlist は `["claude", "codex"]` の2つだけをハードコードしている。

- `crates/cli/src/mcp/runtime_model.rs:27` — `RuntimeModelSnapshot::capture` が `["claude", "codex"]` を固定 iterate し、`sakana-ai` を一切候補にしない。
- `crates/cli/src/mcp/runtime_model.rs:124` — `matches!(*value, "claude" | "codex")` の検証も同じ2値に固定。
- `crates/cli/src/mcp/tools/session.rs:217,487` — `session_dispatch`/`session_create` の JSON Schema `"enum":["claude","codex"]` が同じ2値をハードコード。

`WorkspaceAgentConfig::models(&self, runtime: &str)`（`crates/core/src/infrastructure/runtime_model.rs:81`）は任意の runtime 文字列キーを受け付ける構造であり、workspace の `.usagi/config.toml` に `[agents.sakana-ai].models` を設定しても、`RuntimeModelSnapshot::capture` が `"sakana-ai"` を呼ばないため構造的に到達不能になっている。

この差は単なる表記揺れではなく、**MCP 経由（root/コーディネータ session が使う経路そのもの）では `codex-fugu`（sakana-ai）を選んで agent 起動できない**という機能ギャップである。daemon 側は既に対応しているため、フロントの MCP 面だけが取り残されている。

## 対象

- `RuntimeModelSnapshot::capture` が iterate する runtime 一覧を、`AdapterRegistry` が実際に登録する profile 一覧（`claude` / `codex` / `sakana-ai`）に追従させる。ハードコードした配列を持つ代わりに、単一の SSoT（`AdapterRegistry` の profile catalog、あるいは同じ語彙を core の 1 か所に集約したもの）から導出する。
- `runtime_model.rs:124` の検証と `session.rs:217,487` の JSON Schema enum も同じ SSoT を参照するよう統一する。
- 将来 4 つ目の profile が `AdapterRegistry` に追加された場合、MCP 側の allowlist ハードコードを個別に直す作業が発生しないようにする。

## 受入条件

- [ ] workspace 設定に `[agents.sakana-ai].models` があり `codex-fugu` が PATH 上にあるとき、`session_dispatch` / `session_create` の `agent` schema に `runtime: "sakana-ai"` の分岐が現れる。
- [ ] `RuntimeModelSnapshot` の runtime 一覧が daemon の `AdapterRegistry` 登録 profile と手動で同期しなくても一致することを保証するテストがある（例: 両者が同じ定数/関数を参照するユニットテスト、または `AdapterRegistry` の profile 一覧を返す関数を core に切り出し両面が参照する）。
- [ ] 既存の `claude` / `codex` の挙動（allowlist・executable 有無フィルタ）に regression がない。
- [ ] `document/07-mcp.md` あるいは関連ドキュメントに sakana-ai が MCP からも選択可能であることが反映される。
