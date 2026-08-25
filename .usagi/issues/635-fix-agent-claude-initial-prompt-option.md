---
number: 635
title: fix(agent): Claude initial prompt を option として解釈させない
status: done
priority: medium
labels: [review, security, daemon, agent, claude, argv]
dependson: []
related: [253]
created_at: 2026-08-02T23:14:00.076228+00:00
updated_at: 2026-08-03T00:14:29.726762+00:00
---

## Finding（medium / argv trust boundary）

### 脅威モデルと対象

dispatch/delegateから来るopening promptはuntrustedなopaque dataであり、provider CLIのoption/configとして再解釈してはならない。

`crates/daemon/src/usecase/claude.rs::render_plan` はheadlessで `--print`、optional `--model` の後へ `request.initial_prompt` をそのままpushし、option terminator `--` を置かない。Codex adapterは同じ境界でprompt前に `--` を置く。

### 発生条件・影響・根拠

promptが `--version`、`--settings=...`、`--dangerously-skip-permissions`、`-c` 等で始まるとprovider optionとして解釈され得る。実機 `claude --print --version` はpromptを処理せずversionを出してexit 0した。taskを一切実行しないfalse successに加え、settings/permission optionはdaemon-owned configの意味を変え得る。

`LaunchPlan::new` はNUL/empty/secret markerだけを検査しleading hyphenを許すため、下位層でも止まらない。

### effect-zero 条件

initial promptの全bytesは一つのopaque positional valueとしてproviderへ渡り、version表示、config load、permission変更、subcommand選択などのoption effectを0件にする。不明なprovider argv grammarではlaunch自体を拒否する。

## 修正方針

Claudeのinitial prompt直前にproviderが保証するoption terminatorを置く。対応しないversionではstdinまたは専用prompt flag等のtyped transportを使う。provision arguments、resume、model、promptの順序をadapter contractとして固定する。

## 必要な回帰テスト

`--version`、`--settings=/tmp/x`、`--dangerously-skip-permissions`、`-c` を各1個のopaque promptとしてfixtureへ渡し、option effectが無いことを確認する。interactive/headless/new/resumeのargv matrixも固定する。

## 既存 issue との差分
