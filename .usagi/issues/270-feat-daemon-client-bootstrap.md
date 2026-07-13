---
number: 270
title: feat(daemon): client daemon bootstrap を合成ルートに統一する
status: done
priority: high
labels: [daemon, ipc, cli, tui, mcp]
dependson: [216, 220]
related: [207, 213, 268]
created_at: 2026-07-13T03:00:00+00:00
updated_at: 2026-07-13T03:20:00+00:00
---

## 目的

`usagi` の各入口が managed operation を始める前に、同じ daemon bootstrap を通過するようにする。既存 daemon の endpoint を再利用し、不在時だけ lifecycle `start` を一度要求して active IPC endpoint の公開まで待つ。daemon 不通を local state / PTY 実行で代替しない。

## スコープ

- 合成ルートに endpoint connect、autostart、bounded readiness poll、safe failure を集約する。
- TUI 起動、CLI の daemon request、MCP server が共通 bootstrap を使う。
- lifecycle `start` の成功終了を確認し、endpoint の不正・draining・接続拒否は起動済み daemon として扱い、別 daemon を起動しない。
- generic seam の unit test で既存 endpoint 再利用、endpoint 不在時の単一 start と readiness、start failure、timeout、non-absence failure を固定する。
- daemon server composition / IPC protocol は変更しない。

## 受け入れ条件

- `usagi`、daemon-owned CLI operation、`usagi mcp` は operation 前に同一 bootstrap を使う。
- endpoint が利用可能なら child process を起動しない。endpoint locator が存在するが接続不能な場合も replacement を起動しない。
- absent locator の競合では lifecycle start に委ね、readiness 成功時だけ client を作る。
- 失敗は利用面に safe message で示され、local fallback、blind retry、二重 spawn をしない。
- 実装済み daemon contract documentation と test を同じ PR に含める。
