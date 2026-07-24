---
number: 537
title: feat(agent): v2 で Claude 起動を claude-sandbox と --settings フックに live 配線する
status: todo
priority: high
labels: [agent, daemon, security, claude, sandbox]
dependson: []
related: [530]
created_at: 2026-07-24T13:01:00.604658+00:00
updated_at: 2026-07-24T13:01:00.604658+00:00
---

## 背景

[#530](530-feat-agent-v2-claude-guard-workspace-os-sandbox.md) の第 2 段で、OS sandbox の**機構**を実装・ユニットテストした:

- `usagi claude-sandbox` コマンド（fail-closed の platform sandbox launcher）と純粋な計画ロジック `usagi-core::usecase::claude_sandbox`（macOS `sandbox-exec` profile / Linux `bwrap`。firmlink `/private` 対応・末尾スラッシュ正規化済み）。
- `--settings` の hook JSON 材料化 `usagi_daemon::usecase::claude::scoped_settings_json`（`PreToolUse`→`guard-workspace`（session のみ）＋ライフサイクル→`agent-phase`）。
- 子を launcher で包む配管 `SpawnProvision::sandbox_launcher` / `set_sandbox_launcher` と、合成ルートの spawner がそれを尊重する経路。

ただし **これらはまだ live な Claude 起動経路に接続していない**。`RootClaudeProvisioner` は従来どおり `--mcp-config` / `--allowedTools` だけを出力する。live 化すると既存の E2E テスト（~17 の claude 起動サイト・4 ファイル: `tests/cli_tui_pty.rs` / `tests/agent_ipc_e2e.rs` / `tests/mcp_e2e.rs`＋`tests/support/mcp.rs` / `crates/daemon/tests/agent_real_pty.rs`）が壊れるため、テスト基盤対応と併せて本 issue で行う。

壊れる理由:
- **Linux CI に `bwrap` が無い**ため、always-on sandbox が fail-closed で全 claude 起動を拒否する。
- 実 `sandbox-exec`（macOS）下では、fixture の出力先（count / log ファイル）が writable root の外で、`TMPDIR` も子へ伝播していないため書き込みが deny される。

## やること

- **live 配線**: `RootClaudeProvisioner::provision` で `mode`（`context.scope.session_id` 由来）を判定し、`claude_mcp_arguments` に `--settings scoped_settings_json(usagi, include_guard=session)` を足し、`SpawnProvision::set_sandbox_launcher` で `claude-sandbox --mode <mode> --writable-root … --` を包む。writable root は起動 cwd・workspace の `.usagi`・Git common dir（`workspace_root/.git`）・usagi state（`data_home`）。
- **`TMPDIR` 伝播**: sandbox された子が自身の一時領域へ書けるよう、`TMPDIR` を agent 子プロセスへ伝える（`TERMINAL_ENVIRONMENT_VARIABLES` への追加、または claude 固有 env の注入）。
- **E2E テスト基盤**: sandbox backend seam を用意する。`resolve_sandbox_backend`（合成ルート）にテスト用の passthrough backend を差し込めるようにし、Linux CI でも claude が起動できるようにする（`bwrap` 不在でも通す）。あるいは fixture の出力先を writable root 内へ寄せる。4 つの fixture 系（`write_agent_fixtures` / `materialize_fixture_script` / `agent_ipc_e2e` / `agent_real_pty`）を一貫して更新する。
- **ドキュメント**: `document/02-architecture.md` の「OS sandbox launcher」節を live 配線後の実態へ更新する。テストは 100% カバレッジ維持。

## 完了条件

- session 起動の Claude が `usagi claude-sandbox --mode session -- claude …` 経由で起動し、`PreToolUse` に `guard-workspace` が配線される（#530 完了条件 1）。
- root 起動の Claude が `--mode root` で起動し、project root と一時領域への書き込みだけが許可される（#530 完了条件 2）。
- backend 不在・未対応 platform では無保護起動しない（#530 完了条件 3）。
- 全 E2E テスト（上記 4 ファイル）が Linux CI で green。

## 参考

- 機構の実装（第 2 段）: [#530](530-feat-agent-v2-claude-guard-workspace-os-sandbox.md)
- phase 報告 IPC（第 3 段の別軸）: [#531](531-feat-agent-v2-agent-phase-daemon-phase-ipc.md)
