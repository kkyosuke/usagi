---
number: 494
title: fix(v1/mcp): root session の tracked memory mutation を禁止する
status: in-progress
priority: medium
labels: [review, v1, mcp, memory, security]
dependson: []
related: [105, 106, 404]
parent: 453
created_at: 2026-07-20T12:07:07.043815+00:00
updated_at: 2026-07-20T22:19:02.787752+00:00
---

## 問題・影響

出荷中 v1 の `v1/src/presentation/mcp/usagi.rs::ROOT_FORBIDDEN_TOOLS` は `memory_save` / `memory_delete` を禁止せず、comment/test は `.usagi/memory/` が gitignored と仮定する。一方 `v1/src/infrastructure/gitignore.rs::USAGI_GITIGNORE` は `!/memory/` で Markdown memory を追跡対象にするため、root coordinator の MCP call が main worktree を dirty にできる。

## 成立条件 / 再現フロー

root session の v1 MCP から `memory_save` / `memory_delete` を実行して `git status` を確認する。guard は許可し、tracked memory の追加/削除が root branch に現れる。

## 対象責務と非対象

root MCP mutation policy と gitignore/docs の整合を対象とし、root では memory write/delete を禁止、session では許可する方針を採る。memory read/search、root memory を untracked store へ全面移行、root/v2 #404 は非対象。

## 受入条件

- [ ] root context の `memory_save` / `memory_delete` を side effect 前に typed forbidden error で拒否する。
- [ ] root の read/search と session worktree の memory mutation は既存 capability に従い維持する。
- [ ] mutating store tool の policy table と gitignore tracked status が一致する。
- [ ] malformed/unknown context は root write を許可せず fail closed にする。

## 必須回帰テスト

実 Git repo で root の全 mutating store tool 後に `git status` unchanged、session memory write は session branch changed、read/search、malformed context effect 0 を検証する。

## docs / 移行影響

v1 MCP/memory docs と guard comment を tracked memory の実契約へ修正する。既存 root の未コミット memory は自動削除せず、session へ移す手順を案内する。
