---
number: 625
title: test(cli): MCP・CLI decision path を coverage 対象へ戻す
status: todo
priority: medium
labels: [review, v2, test, coverage, cli, mcp]
dependson: [484]
related: [453]
created_at: 2026-08-02T23:07:31.755387+00:00
updated_at: 2026-08-02T23:07:31.755387+00:00
---

## Finding（P2 / coverage gate の盲点）

### path / symbol

`coverage-off-allowlist.json` で owner `root-cli` / reason `migration_debt` とされた `crates/cli/src/**` の22 item。主な decision path は次のとおり。

- `crates/cli/src/cli/mod.rs::{Command::into_handler, Session::run, run}`
- `crates/cli/src/mcp/serve.rs::{serve_with_client_and_snapshot, handle_line_with_client, respond, tools_list_result, tools_call, execute_tool, store_tool_call, apply_caller_policy, resources_read}`
- `crates/cli/src/cli/commands/{update,version}.rs::run`

### 発生条件

これらの parser、route selection、schema projection、caller credential injection、error mapping を壊して `cargo llvm-cov --workspace` を実行する。item 全体が `#[coverage(off)]` のため、既存 unit/E2E が一部の経路を実行していても coverage gate は未到達 branch/function を報告しない。

### 影響

`document/07-mcp.md` が SSoT とする「tools/list と tools/call の同一 descriptor registry」「schema による runtime validation」「caller policy」「protocol error mapping」の退行が100% gateから隠れる。未検証 error path や新しい route branch が追加されても、coverage 100% を維持したように見える。

### 具体的根拠

- `document/06-conventions.md#coverageoff-例外` は parser、validation、error mapping を許可理由から明示的に除外する。
- `document/08-coverage.md#rootcli-の内訳` も CLI command/MCP parser・error mapping を削除対象と明記する。
- registry の22 itemは許可理由ではない凍結 debt `root-cli-follow-up` に紐づくが、その名前の既存 issue は `issue_search` で0件だった。
- `crates/cli/src/mcp/serve.rs` には多数の schema/protocol/route unit test、`tests/mcp_e2e.rs` には shipping stdio/daemon E2E が既にあり、少なくとも decision logic 全体を除外し続ける根拠にならない。

### 修正方針

テスト可能な parser、validation、route selection、projection、error mapping から `#[coverage(off)]` と registry entry を削除する。stdio/current-cwd/process の実 IO だけを最小関数へ分離し、残す場合は `real_io` / `composition` と具体的 test evidence で再登録する。

### 必須回帰テスト

- JSON-RPC envelope、initialize state、tools/list/call schema、dynamic runtime/model snapshot の全 branch。
- descriptor route と caller policy の全組、credential 有無、invalid/missing/additional arguments。
- daemon/store success、protocol/transport/store error と side-effect detail mapping。
- CLI command parse/dispatch、session action payload、invalid argv、stdout/stderr/exit code。
- `ruby scripts/coverage-off-lint.rb` と workspace line/function 100% gate。

### docs / migration

`document/08-coverage.md` の件数・返済結果を更新する。wire、永続データ migration はない。
