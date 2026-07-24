---
number: 530
title: feat(agent): v2 で Claude 起動へ guard-workspace フックと OS sandbox を配線する
status: in-progress
priority: high
labels: [agent, daemon, security, claude, sandbox]
dependson: []
related: []
created_at: 2026-07-24T03:49:25.984667+00:00
updated_at: 2026-07-24T10:05:43.019099+00:00
---

## 背景

v1 PR #1253 相当（[#467](467-fix-v1-security-claude-workspace-os-sandbox-fail-closed-guard.md) の書き込み範囲緩和版）を v2 へ移植中。第 1 段として **workspace guard の中核ロジックと enforcing な `guard-workspace` フック**を実装した（`usagi-core::usecase::workspace_guard` ＋ `usagi-cli` の `cli/hooks/guard_workspace`）。本 issue はその残りの配線・保護層を扱う。

第 1 段では `guard-workspace` コマンド自体は enforcing になったが、**Claude 起動時に実際にフックとして配線されていない**（v2 の Claude provisioner は `--mcp-config` / `--allowedTools` しか出力せず、`--settings` による hook 材料化がまだ無い）。また OS sandbox も未実装。

## やること

- **hook-settings の材料化**: v2 の Claude adapter / provisioner（`crates/daemon/src/usecase/claude.rs` と合成ルート `src/runtime/daemon.rs` の `RootClaudeProvisioner`）が Claude の `--settings` JSON を生成し、`PreToolUse` に `usagi guard-workspace`、ライフサイクルに `usagi agent-phase <phase>` を配線する。
  - session 起動: `PreToolUse` 配列へ phase 報告と `guard-workspace` を並べて差し込む。
  - root 起動: `guard-workspace` は差し込まず、OS sandbox policy に委ねる（root モードの判定は cwd 由来なので guard 自体は root でも安全に働くが、v1 同様 root は sandbox を主境界にする）。
- **OS sandbox launcher（`usagi claude-sandbox`）**: fail-closed の platform sandbox で Claude を起動する隠しコマンドを追加する（v1 `presentation/cli/claude_sandbox.rs` 相当）。
  - macOS は `/usr/bin/sandbox-exec`、Linux は `bwrap`。利用不能／Windows では無保護フォールバックせず起動拒否。
  - writable root: 起動 cwd（session worktree または project root）、workspace の `.usagi`、Git common dir、Claude/usagi の state dir、`$TMPDIR`・`/tmp`・`/var/tmp`、macOS の Keychain / MDS cache。
  - `agent-phase` の実記録（daemon への phase 報告）も併せて実装する（現状スタブ）。
- **writable roots の受け渡し**: v2 の `LaunchScope`（`session_id: Option`）から session / root を判定し、sandbox に渡す writable roots を組み立てる。
- ドキュメント（`document/02-architecture.md` のフックコマンド節・必要なら orchestration 節）を実配線後の実態に更新する。テストは 100% カバレッジ維持。

## 完了条件

- session 起動の Claude が `usagi claude-sandbox --mode session -- claude …` 経由で起動し、`PreToolUse` に `guard-workspace` が配線される。
- root 起動の Claude が `--mode root` で起動し、project root と一時領域への書き込みだけが sandbox policy 上許可される。
- sandbox backend 不在／未対応 platform では Claude を無保護で起動しない（fail-closed）。
- `agent-phase` が phase を daemon へ報告する。

## 参考

- v1 実装: [#467](467-fix-v1-security-claude-workspace-os-sandbox-fail-closed-guard.md)、PR #1253（v1、CLOSED）
- 第 1 段（本 issue の前段）で入った guard 中核: `usagi-core::usecase::workspace_guard` / `usagi-cli` `cli/hooks/guard_workspace`
