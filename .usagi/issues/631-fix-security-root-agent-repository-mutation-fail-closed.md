---
number: 631
title: fix(security): root Agent の repository mutation を fail-closed にする
status: done
priority: high
labels: [review, security, orchestration, daemon, agent, claude, codex, sandbox]
dependson: []
related: [107, 377, 537, 630]
created_at: 2026-08-02T23:13:59.705211+00:00
updated_at: 2026-08-03T02:26:41.432042+00:00
---

## Finding（P0 security）

### 脅威モデルと対象

workspace-root Agent は coordinator であり、repository変更は必ずsession branchへ委譲する。provider output、prompt、repository configは untrusted とし、instructionだけをsecurity boundaryにしない。

- `src/runtime/daemon.rs::claude_writable_roots` は root Claudeのcwdであるproject rootをwritableにし、`claude_settings_arguments` はroot modeで `guard-workspace` を配線しない。
- `crates/daemon/src/usecase/codex/mod.rs::render_plan` は interactive rootを `--sandbox workspace-write --ask-for-approval never`、headlessを `--dangerously-bypass-approvals-and-sandbox` で起動する。
- `crates/core/src/usecase/workspace_guard.rs::root_command_is_read_only` も、配線した場合なおGit external helperを許す。`git -c diff.external=touch diff --ext-diff ...` と repository設定のdiff driverはallowlisted `git diff` から任意processを起動できる。

隔離した一時repositoryで上記 external diff が `PWNED` を作り、実 `guard-workspace` は同commandをstdout空・exit 0で許可することを確認した。

### 影響

root Agentがsrc/docs/issues/indexを直接編集し、git mutationや任意helperをapprovalなしで実行できる。session branch、single writer、Draft PR、root mutation guardを迂回する。

### effect-zero 条件

root scopeのEdit/Write、redirect、filesystem mutator、Git worktree/index/ref mutation、外部helper起動は拒否され、worktreeとGit common dirのbytesが不変でなければならない。read-only Gitはuntrusted config/attribute/pager/textconv/ext-diff/hookを実行しない経路だけを許す。

## 修正方針

- Claude/Codexともroot専用policyを持ち、repository checkoutとGit common dirをread-onlyにする。approvalで許可するのではなくdenyする。
- Claude rootにもenforcing guardを配線し、OS sandboxでもproject rootをwritable rootから外す。
- Codexのroot/session sandbox・approval設定を分離し、headless bypassをrootで使わない。
- raw BashのGit名 allowlistだけに依存せず、sanitized environment/configで固定argvを実行するread-only adapterを検討する。少なくとも `-c` / `--config-env`、pager、external diff/textconv、upload-pack等のprocess起動optionは拒否する。

## 必要な回帰テスト

Claude/Codex rootのEdit、Bash write、git add/commit、redirect、symlink write、inline `diff.external`、repository-config diff driverが非0かつrepo/index byte不変となるproduction E2Eを追加する。read-only status/log/diffとsession delegationは成功させる。

## 既存 issue との差分

#107 はroot guard契約そのものだが、現v2 production wiringはClaude rootに未配線でCodexに同等境界がない。#377 はMCP toolsのapproval省略でfilesystem境界は緩和しない契約、#537 は現Claude root wiringの導入元である。
