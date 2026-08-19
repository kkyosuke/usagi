---
number: 705
title: fix(sandbox): session Claude の global config (`~/.claude.json`) を writable にする
status: done
priority: high
labels: []
dependson: []
related: [702]
created_at: 2026-08-19T22:43:07.017935+00:00
updated_at: 2026-08-19T22:43:11.010200+00:00
---

## 症状

#702（PR #1520）で `CLAUDE_CONFIG_DIR` の worktree 上書きを外し、利用者本人の `~/.claude` を writable root に
配ったあとも、**session の Claude は起動のたびに folder trust（"Is this a project you trust?"）と初回フローを
聞いてくる**。permission mode・MCP 承認も持続しない。

## 原因

Claude の onboarding 完了・folder trust・per-project MCP 承認は `~/.claude`（state directory）の中ではなく、
**隣の `~/.claude.json`** にある。sandbox は `~/.claude` の subtree しか writable にしていないため、この file は
**読めるが書けない**。読めるので `hasCompletedOnboarding` は効くが、`projects[<cwd>].hasTrustDialogAccepted` の
保存が拒否され、trust dialog が毎起動やり直しになる。

保存は atomic で、`$HOME` 直下に lock と temp を作る（実測）:

```
~/.claude.json                       本体
~/.claude.json.lock                  保存 lock
~/.claude.json.tmp.<pid>.<random>    temp（rename で本体に被せる）
~/.claude.json.backup.<ms>           backup
```

したがって「その 1 ファイル」を許可しても足りず、**path prefix** の grant が要る（`(literal "~/.claude.json")`
だけの profile では保存が通らないことを実測で確認）。

## 対応

- `DefaultModel::global_config_prefix()` を SSoT に、launcher が exec する program から config prefix を決める
  （Claude: `.claude.json` / Codex・codex-fugu: config は state directory 内なので無し）。
- macOS: profile に `(allow file-write* (regex #"^<prefix>"))` を足す（path の regex メタ文字は escape）。
  `$HOME` 全体は writable にしない。
- Linux: `bwrap` は mount 単位で prefix を表現できないため、`--bind-try <prefix>` で config 本体だけ再 bind する。
- daemon 側の policy gate も同じ program から prefix を決め、保護対象 workspace / Git common dir との重なりを拒否する。

## 検証

`usagi claude-sandbox --mode session … -- claude` を実 PTY で起動し、trust dialog を受理したあとに
`~/.claude.json` の `projects[<cwd>]` が書かれることを確認する（修正前は書かれない）。

## 残課題

Linux は `$HOME` directory 自体が read-only のままなので、lock / temp を要する保存経路は通らない。
mount 単位の grant で prefix 相当を表現する方法（`$HOME` を writable にして既存 entry を ro-bind し直す等）は別 issue。
