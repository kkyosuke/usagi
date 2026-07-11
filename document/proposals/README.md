# 設計提案（proposals）

> [ドキュメント目次](../README.md)

`document/` 直下の番号付きドキュメント（`01-` …）・`design/`・`data/` は**現在のビルドで動作する仕様の正本**であり、
[06-conventions.md#記載実装済み](../06-conventions.md#記載実装済み) に従って未実装の内容を含めません。

一方、まだ実装されていない**運用モデル・機構の設計判断**を記録したいことがあります。これを spec に混ぜると
「どこまで本当か」が読者に判断できなくなるため、**設計提案はこの `proposals/` に分離**します。実装が進んで挙動が
確定したら、その内容を正本（`04-orchestration.md` など）へ畳み込み、提案は撤去またはリンクだけ残します。
ロードマップ（実装タスク）は issue ストア（`.usagi/issues/`）で追跡します。

## 一覧

| # | ドキュメント | 内容 | 状態 |
|---|---|---|---|
| 1 | [01-root-orchestration.md](01-root-orchestration.md) | 自律オーケストレーション運用モデル（root＝オーケストレーション専任・変更は必ず session） | 正本へ畳み込み済み（#105・[04-orchestration.md](../04-orchestration.md#自律オーケストレーション運用モデル)） |
| 2 | [02-daemon.md](02-daemon.md) | daemon（常駐プロセス）による agent ライフサイクルの TUI 非依存化（PTY 所有を daemon へ移し TUI をクライアント化） | 提案（実装中・Epic #159 / Step 1 #160・Step 2 #161・Step 3a #163・Step 3b-1 #164・Step 3b-2 #165・Step 3b-3 #166 済み） |
| 3 | [03-durable-issue-orchestrator.md](03-durable-issue-orchestrator.md) | 単一の統括 session が issue DAG と worker を所有する永続・冪等オーケストレータ | 提案（実装 issue #182–#186） |
| 4 | [04-sccache-rust-builds.md](04-sccache-rust-builds.md) | sccache による Rust ビルド高速化の導入計画 | 提案（実装 issue #191） |
| 5 | [05-session-lifecycle.md](05-session-lifecycle.md) | session の作成・初期化・削除・失敗を `state.json` へ一元化する永続 lifecycle | 提案 |
