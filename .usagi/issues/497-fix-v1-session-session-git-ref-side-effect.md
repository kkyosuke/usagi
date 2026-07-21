---
number: 497
title: fix(v1/session): session 名を Git ref 規則で side effect 前に検証する
status: done
priority: medium
labels: [review, v1, session, git]
dependson: []
related: [30, 49, 361, 370]
parent: 453
created_at: 2026-07-20T12:07:08.029028+00:00
updated_at: 2026-07-21T02:19:10.484546+00:00
---

## 問題・影響

出荷中 v1 の `v1/src/usecase/session/mod.rs::{name_format_error,branch_name}` は slash、`.`/`..`、leading hyphen 程度しか拒否せず、`usagi/<name>` に対する Git ref 規則を満たさない `..` 含有、`@{`、`.lock`、trailing dot、control/space、`~^:?*[` を許す。filesystem/setup effect 後の Git command で失敗し、partial session を残せる。

## 成立条件 / 再現フロー

各 invalid pattern を CLI/TUI/MCP の session create に渡す。local validation を通り、directory/config/setup の一部を作った後 `git branch/worktree` で拒否される。

## 対象責務と非対象

全 v1 entrypoint が共有する session name→branch ref validator と side effect 前 validation を対象とする。表示名、root/v2 の naming policy、既存 invalid session の自動 rename は非対象。

## 受入条件

- [ ] final `usagi/<name>` を Git `check-ref-format` 相当の共有 validator で検査する。
- [ ] CLI/TUI/MCP/orchestrator の全入口が filesystem/Git/setup effect 前に同じ validator を使う。
- [ ] invalid input は safe/一貫した error を返し、file/branch/worktree を作らない。
- [ ] valid Unicode/長さ/既存 namespace conflict の policy を明示する。

## 必須回帰テスト

Git invalid pattern table、valid boundary、全 entrypoint parity、branch namespace conflict、effect counter 0、実 `git check-ref-format` parity を検証する。

## docs / 移行影響

v1 session naming docs と UI help を更新する。既存 invalid session は読取/削除可能に保ち、新規作成だけ拒否する migration とする。
