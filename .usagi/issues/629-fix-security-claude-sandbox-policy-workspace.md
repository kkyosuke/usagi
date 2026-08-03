---
number: 629
title: fix(security): Claude sandbox の policy 環境を workspace 設定から分離する
status: in-progress
priority: high
labels: [review, security, daemon, agent, claude, sandbox, environment]
dependson: []
related: [530, 537, 602, 608, 630, 632]
created_at: 2026-08-02T23:13:59.519131+00:00
updated_at: 2026-08-02T23:55:35.243016+00:00
---

## Finding（P0 security）

### 脅威モデルと対象

workspace の内容、session Agent が実行する Bash、`<workspace>/.usagi/settings.json` の environment binding は untrusted とする。一方、sandbox backend の executable と writable-root policy は Claude 起動前に daemon が確定する trust boundary である。

対象は `src/runtime/user_env.rs::UserEnvironment::{configured,resolved}`、`src/runtime/daemon.rs::{RootClaudeProvisioner::provision,launch_environment,claude_writable_roots}`、`crates/daemon/src/usecase/runtime.rs::SpawnProvision::compose_environment`、`src/runtime/cli.rs::{claude_sandbox,resolve_sandbox_backend}`、`crates/core/src/usecase/claude_sandbox.rs::writable_roots`。

### 発生条件・影響・根拠

workspace 設定の任意 environment 名が Agent の完全な spawn environment に入り、PTY は ambient env を消してその環境で sandbox launcher 自身を起動する。launcher は同じ `PATH` から Linux の `bwrap` を探索し、同じ `TMPDIR` / `HOME` を writable-root policy に使う。

- Linux で `PATH=<session内>/fake-bin` とすると、sandbox 適用前に fake `bwrap` を trusted backend として exec できる。fake backend は credential / secret を読み、Claude を無拘束で起動できる。
- `TMPDIR=/` は macOS profile の `(subpath "/")` または Linux の read-write bind になり、write confinement を消す。
- session sandbox は `<workspace>/.usagi` を writable にするため、稼働中 Agent が次回 dispatch 用 settings を変更できる。

正本は OS sandbox を hard boundary とし、backend 不在・不正時の無保護 fallback を禁止する。現状は untrusted policy input で product spawn に到達する。

### effect-zero 条件

sandbox policy input が trusted source と証明できない、backend identity が不明、または writable root が `/`・session workspace の保護対象 ancestor・symlink aliasへ解決される場合、secret resolution、PTY reservation、backend exec、product spawnをすべて 0 件で拒否する。

## 修正方針

- launcher-control environment と product environment を別チャネルに分け、workspace binding から最低でも `PATH` / `TMPDIR` / `HOME` / `USAGI_CLAUDE_SANDBOX_PASSTHROUGH` を除外する。
- Linux backend は trusted bootstrap で absolute canonical executable として確定し、Agent child の `PATH` で再探索しない。
- writable root は absolute/canonical path、node type、symlink、owner、protected ancestorを検証し、session mode の `/`、workspace root、その祖先を拒否する。
- failure は typed error とし、秘密値や attacker executableへ到達させない。

## 必要な回帰テスト

- workspace env の fake `PATH`、`TMPDIR=/`、`HOME=/`、symlink TMPDIR、passthrough 変数を table-driven に検証する。
- fake backend marker、secret resolver、PTY/backend exec が 0 件であることを固定する。
- macOS profile / Linux bind argv に `/` や保護対象 ancestor が writable root として出ないことを確認する。
- session Bash → settings mutation → second dispatch の E2E で parent sentinel と credential が保護されることを確認する。

## 既存 issue との差分

#530 / #537 は sandbox と TMPDIR 伝播の導入、#602 は保存済み session name の teardown escape、#608 は data-home の導出を扱う。workspace environment が launcher policy と backend identityを上書きする経路は未対応である。
