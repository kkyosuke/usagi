---
number: 620
title: feat(tui): session role editor・picker・badge を daemon projection で実装する
status: done
priority: medium
labels: [tui, session, role]
dependson: [619]
related: []
created_at: 2026-08-01T01:03:06.183171+00:00
updated_at: 2026-08-02T23:40:33+09:00
---

## 背景

#619 の domain/daemon/MCP/CLI 縦切りでは role assignment の権威を `ManagedSession.role_id` に一本化した。現行 TUI は daemon lifecycle row を永続 UI 注釈 `SessionRecord` へ投影して sidebar を構築するため、そこへ role を追加すると assignment が `sessions.json` と `state.json` の二重正本になる。

## 依存順

1. daemon の `role_id` / `role_summary` を `SessionRecord` と分離した UI-only projection として controller state へ運ぶ。
2. create form が effective session-scope catalog を read-only に取得し、default と候補を picker 表示する。submit は role ID だけを `SessionCreateIntent` から daemon へ送る。
3. sidebar row に safe role ID badge を描画する。catalog 不正時も lifecycle 操作を維持する。
4. Global / Workspace editor は versioned `roles.toml` を lossless/atomic に更新し、validation error を inline 表示する。

## 受け入れ条件

- role metadata は UI state にだけ保持し `state.json` へ保存しない。
- picker は session scope role のみを表示し default を選択する。
- badge は role ID のみで authorization/lifecycle 判断に使わない。
- editor、picker、badge の reducer/render/production seam test を追加する。
