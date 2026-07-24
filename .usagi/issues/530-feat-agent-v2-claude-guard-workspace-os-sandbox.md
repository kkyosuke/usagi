---
number: 530
title: feat(agent): v2 で Claude 起動へ guard-workspace フックと OS sandbox を配線する
status: in-progress
priority: high
labels: [agent, daemon, security, claude, sandbox]
dependson: []
related: [531, 537]
created_at: 2026-07-24T03:49:25.984667+00:00
updated_at: 2026-07-24T13:01:47.981406+00:00
---

## 背景

v1 PR #1253 相当（[#467](467-fix-v1-security-claude-workspace-os-sandbox-fail-closed-guard.md) の書き込み範囲緩和版）を v2 へ移植中。第 1 段として **workspace guard の中核ロジックと enforcing な `guard-workspace` フック**を実装した（`usagi-core::usecase::workspace_guard` ＋ `usagi-cli` の `cli/hooks/guard_workspace`）。本 issue はその残りの配線・保護層を扱う。

第 1 段では `guard-workspace` コマンド自体は enforcing になったが、**Claude 起動時に実際にフックとして配線されていない**（v2 の Claude provisioner は `--mcp-config` / `--allowedTools` しか出力せず、`--settings` による hook 材料化がまだ無い）。また OS sandbox も未実装。

## 進捗（第 2 段: OS sandbox 機構）

OS sandbox の**機構**を実装・ユニットテストした:

- `usagi claude-sandbox` コマンド（fail-closed の platform sandbox launcher）＋純粋計画 `usagi-core::usecase::claude_sandbox`（macOS `sandbox-exec` profile / Linux `bwrap`。firmlink `/private` 対応・末尾スラッシュ正規化を実 `sandbox-exec` で検証）。
- `--settings` の hook JSON 材料化 `usagi_daemon::usecase::claude::scoped_settings_json`（`PreToolUse`→`guard-workspace`（session のみ）＋ライフサイクル→`agent-phase`）。
- 子を launcher で包む配管 `SpawnProvision::sandbox_launcher` / `set_sandbox_launcher`。

**これらはまだ live な Claude 起動経路には接続していない**（`RootClaudeProvisioner` は従来どおり `--mcp-config` / `--allowedTools` だけを出力）。live 化は既存 E2E（~17 claude 起動サイト・4 ファイル）を壊す（Linux CI に `bwrap` 無し／実 sandbox 下で fixture 出力が writable root 外・`TMPDIR` 未伝播）ため、E2E テスト基盤対応と併せて **[#537](537-feat-agent-v2-claude-claude-sandbox-settings-live.md)** に切り出した。`agent-phase` の daemon 報告は **[#531](531-feat-agent-v2-agent-phase-daemon-phase-ipc.md)**。この issue は #537・#531 完了までは `done` にしない。

## やること

- **hook-settings の材料化**: `--settings` JSON を生成し `PreToolUse` に `guard-workspace`、ライフサイクルに `agent-phase <phase>` を配線する。→ builder 実装済み。live 配線は [#537](537-feat-agent-v2-claude-claude-sandbox-settings-live.md)。
  - session 起動: `PreToolUse` 配列へ phase 報告と `guard-workspace` を並べて差し込む。
  - root 起動: `guard-workspace` は差し込まず OS sandbox policy に委ねる。
- **OS sandbox launcher（`usagi claude-sandbox`）**: fail-closed の platform sandbox で Claude を起動する隠しコマンド。→ 実装済み。
  - macOS は `/usr/bin/sandbox-exec`、Linux は `bwrap`。利用不能／Windows では無保護フォールバックせず起動拒否。→ 実装済み。
  - writable root: 起動 cwd・workspace の `.usagi`・Git common dir・Claude/usagi の state dir・`$TMPDIR`・`/tmp`・`/var/tmp`、macOS の Keychain / MDS cache。→ 実装済み。
  - `agent-phase` の実記録（daemon への phase 報告）→ **[#531](531-feat-agent-v2-agent-phase-daemon-phase-ipc.md) へ切り出し**（現状スタブ）。
- **writable roots の受け渡し**: `LaunchScope`（`session_id: Option`）から session / root を判定して writable roots を組み立てる。→ 判定ロジック実装済み。live 配線は [#537](537-feat-agent-v2-claude-claude-sandbox-settings-live.md)。
- ドキュメント（`document/02-architecture.md`）を実態に更新する。テストは 100% カバレッジ維持。→ 済。

## 完了条件

- session 起動の Claude が `usagi claude-sandbox --mode session -- claude …` 経由で起動し、`PreToolUse` に `guard-workspace` が配線される。→ 機構は実装済み・live 配線は [#537](537-feat-agent-v2-claude-claude-sandbox-settings-live.md)。
- root 起動の Claude が `--mode root` で起動し、project root と一時領域への書き込みだけが sandbox policy 上許可される。→ 機構は実装済み・live 配線は [#537](537-feat-agent-v2-claude-claude-sandbox-settings-live.md)。
- sandbox backend 不在／未対応 platform では Claude を無保護で起動しない（fail-closed）。✅（第 2 段。planner で実装・ユニットテスト済み）
- `agent-phase` が phase を daemon へ報告する。→ [#531](531-feat-agent-v2-agent-phase-daemon-phase-ipc.md)

## 参考

- v1 実装: [#467](467-fix-v1-security-claude-workspace-os-sandbox-fail-closed-guard.md)、PR #1253（v1、CLOSED）
- 第 1 段（本 issue の前段）で入った guard 中核: `usagi-core::usecase::workspace_guard` / `usagi-cli` `cli/hooks/guard_workspace`
- 第 3 段（live 配線 + E2E 基盤）: [#537](537-feat-agent-v2-claude-claude-sandbox-settings-live.md)
- phase 報告 IPC: [#531](531-feat-agent-v2-agent-phase-daemon-phase-ipc.md)
