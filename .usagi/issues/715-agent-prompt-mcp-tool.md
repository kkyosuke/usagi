---
number: 715
title: Agent 起動 prompt を配線済み MCP tool 系統から組む
status: todo
priority: medium
labels: []
dependson: []
related: []
created_at: 2026-08-23T23:06:26.005949+00:00
updated_at: 2026-08-23T23:06:26.005949+00:00
---

## 背景

Agent launch の system prompt は scope（root / session worktree）と local LLM の有無だけを見ており、
issue / memory の有効・無効を見ていない。実際の tool registry（`usagi mcp`）は Global + Workspace の
`issue_enabled` / `memory_enabled` を解決して `issue_*` / `memory_*` / `session_delegate_issue` を落とすため、
prompt と `tools/list` が同じ設定から出ていない。

さらに、session orchestration の手順は resource `usagi://guides/orchestration` にあるが、
prompt からその存在を一言も指していないため agent が発見できない。

root scope prompt が issue ストアを名指ししていた点も、scope 境界に capability を混ぜた重複だった。

## 変更内容

- prompt 合成を `scope → <tools> → <role>` の 3 fragment に整理する。`<tools>` は配線済み MCP server が
  登録する tool 系統を 1 系統 1 行で述べ、無効な系統は行そのものを落とす（「無い」とも書かない）。
- `<tools>` は tool 名を列挙せず、`tools/list` が正本であることと `usagi://guides/orchestration` への
  ポインタだけを持つ。local LLM の delegation もこの block の 1 行に統合し、末尾 suffix を廃止する。
- root scope prompt から issue の言及を外し、capability の記述を `<tools>` に一元化する。
- daemon の provisioner が Global + **登録済み workspace root** の `.usagi/settings.json` を
  `with_local` で畳んで effective 値を解決する（`usagi mcp` と同一の解決）。読めない場合は既定へ倒さず
  launch を拒否する。
- orchestration guide の前提モデルに「無効な系統は `tools/list` に現れない」不変条件を書く。

## 確認方法

- `cargo test -p usagi-core`（prompt 合成の順序・省略・1 回性）
- `cargo test -p usagi --bin usagi`（argv 契約、workspace 上書き、fail-closed）
- `cargo test -p usagi --test mcp_e2e`（shipping argv の prompt 契約）
