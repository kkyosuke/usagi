# 設計提案（proposals）

> [ドキュメント目次](../README.md)

`document/` 直下の番号付きドキュメント（`01-` …）は**現在のビルドで動作する仕様の正本**であり、
[06-conventions.md#記載実装済み](../06-conventions.md#記載実装済み) に従って未実装の内容を含めない。

一方、まだ実装されていない**構成・機構の設計判断**を記録したいことがある。これを spec に混ぜると
「どこまで本当か」が読者に判断できなくなるため、**設計提案はこの `proposals/` に分離**する。実装が進んで
挙動が確定したら、その内容を正本（`02-architecture.md` など）へ畳み込み、提案は撤去またはリンクだけ残す。
ロードマップ（実装タスク）は issue ストア（`.usagi/issues/`）で追跡する。

v1 時点の設計提案（daemon 化・durable orchestrator など）は退避版
[v1/document/proposals/](../../v1/document/proposals/README.md) にあり、更新しない。

## 一覧

| # | ドキュメント | 内容 | 状態 |
|---|---|---|---|
| 1 | [01-entry-surfaces.md](01-entry-surfaces.md) | 入口面（CLI / MCP）の配置と、daemon を実行の権威とする反映フロー | 提案（クレート構成・dispatch は [02-architecture.md](../02-architecture.md) へ畳み込み済み） |
