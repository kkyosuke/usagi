---
number: 335
title: chore(issues): issue ストアの番号衝突を解消する
status: done
priority: medium
labels: [chore, issues, tooling]
dependson: []
related: []
created_at: 2026-07-17T22:51:02.213392+00:00
updated_at: 2026-07-17T22:54:44.660181+00:00
---

## 背景

`.usagi/issues/` に、同一番号を複数ファイルが持つ「番号衝突」が pre-existing で存在する（`main` にも同状態）。
番号は他 issue の `dependson` / `related` / `parent` から参照されるため、衝突があると参照解決が曖昧になる。

対象の重複番号（`270` は 3 ファイル、他は 2 ファイル）:
`165, 166, 182, 201, 268, 270, 302, 303`

## やること

各重複番号について 1 ファイルを元番号のまま残し、残りを未使用の新番号へ振り直す。
残す基準は「被参照が多い方」または「`created_at` が古い方」。

- ファイル名の番号プレフィックスと frontmatter `number` を一致させる。
- 振り直した issue を参照する全箇所（他 issue の `dependson`/`related`/`parent`、`document/` の `#NNN`）を更新する。
- 新番号は現行最大番号より大きい未使用値から採番する。ただし `#333`（PR #1024）と `#334`（PR #1029）は使用中のため避ける。

## 完了条件

- ファイル名 vs frontmatter の番号が全 issue で一致する。
- 番号重複がゼロになる。
- すべての `dependson`/`related`/`parent` 参照が解決可能である。
- Markdown link check が通る（Markdown 差分がある場合）。

## スコープ外

MCP resource / tool 実装（PR #1024・issue #333）には触れない。
