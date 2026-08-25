---
number: 630
title: fix(security): session sandbox の writable root を own scope に限定する
status: done
priority: high
labels: [review, security, daemon, agent, claude, sandbox, session, filesystem]
dependson: []
related: [537, 602, 629, 631, 632]
created_at: 2026-08-02T23:13:59.615604+00:00
updated_at: 2026-08-03T00:09:45.239755+00:00
---

## Finding（P0 security）

### 脅威モデルと対象

managed session 内の Agent、repository content、Bash/provider output は untrusted とする。session A は own worktree だけを書け、sibling session、root issue store、daemon durable authority は別 writer の trust boundary である。

`src/runtime/daemon.rs::RootClaudeProvisioner::provision` / `claude_writable_roots` は session Claude に own worktreeだけでなく `<workspace>/.usagi` と mode-neutral data-home 全体を writable root として渡す。`crates/cli/src/cli/hooks/guard_workspace.rs::session_deny_reason` は well-formed Bash を無条件で OS sandboxへ委ねる。

### 発生条件・影響・根拠

session Agent が Bash で `<workspace>/.usagi/sessions/<sibling>`、`<workspace>/.usagi/issues`、daemon stateへ書けば、すべて sandbox allow root 内なので OS 境界は拒否しない。

- sibling session の未commit成果や branch stateを破壊できる。
- issue backlogを直接変更し、root/sessionの単一 writer と PR workflowを迂回できる。
- daemon durable stateを改ざんし、各read境界に個別validationがない箇所へ攻撃面を広げる。

既存 test は session roots に `/repo/.usagi` が含まれることと、session Bash が hookを通ることを肯定している。#602 はこの前提から成立する teardown exploitを修正したが、権限分離自体は対象外だった。

### effect-zero 条件

own session scope外を狙うfilesystem effectはsandbox/typed broker境界で拒否され、sibling worktree、tracked issue source、daemon durable bytesが不変でなければならない。単に後段のparserが不正値を拒否することは write effect zero の代替にならない。

## 修正方針

- `.usagi` / data-home 全体を直接 writable rootにしない。
- Agentに必要な issue/session/phase等のmutationはdaemon/MCPのtyped・credential-scoped APIへ寄せる。
- 移行中にfilesystem accessが必要なら、own session専用の最小subtreeだけを明示し、siblings、tracked source、locks、daemon authorityを除外する。
- symlink / firmlink aliasでも保護領域へ到達しない canonical policyを持つ。

## 必要な回帰テスト

- shipping launcherで session A から sibling sentinel、root issue、daemon stateへのwrite/remove/renameを試し、非0かつ全byte不変を確認する。
- own worktree writeと必要なtyped MCP mutationは成功する。
- absolute path、`..`、symlink alias、hardlinkを含むmatrixを固定する。

## 既存 issue との差分

#530 / #537 は broad roots を導入したissue、#602 は保存値の再検証による特定teardown防御である。本issueはsession間・root・daemon authorityのwritable capabilityそのものを分離する。
