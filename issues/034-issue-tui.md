---
number: 034
feature: issue-tui
title: TUI issue コマンド（関連性テーブル・進捗の可視化）
status: todo
priority: medium
category: tui
dependson: [002, 023, 024]
---

# TUI `issue` コマンド（関連性テーブル・進捗の可視化）

## 概要

これまで issue 操作は CLI（[024-issue-cli](024-issue-cli.md)）と MCP（[025-issue-mcp](025-issue-mcp.md)）で
提供してきました。本 issue では、ホーム画面のコマンドモードから使える **TUI 内 `issue` コマンド**を追加し、
TUI から離れずに issue を一覧・参照・更新できるようにします。

加えて、現在 MCP/CLI で提供している CRUD・検索に**留まらず**、

- **関連性をテーブルで表示**: 各 issue の `dependson`（依存）を含めた一覧を表で示し、どの issue が
  どれに依存しているか（ブロック/被ブロック関係）を一目で把握できるようにする。
- **どこまで終わっているかを可視化**: `status`（todo / in-progress / done）の集計・進捗バー・完了率を
  表示し、プロジェクト全体・ラベル単位の進み具合が見えるようにする。

を実現します。`.usagi/issues/`（[023-issue-store](023-issue-store.md)）の index を読み込み、
本リポジトリの [issues/README.md](README.md) の一覧表に相当するビューを TUI 上でライブに見せるイメージです。

## やること

- ホーム画面のコマンドレジストリ（`presentation/tui/home/command.rs`）に `issue` コマンドを追加する。
  - `issue list` — issue 一覧をテーブル表示（number / title / status / priority / dependson）。
  - `issue show <number>` — frontmatter + 本文を右ペインに表示（[033-preview-viewer](033-preview-viewer.md) の
    Markdown レンダリングを活用できれば再利用する）。
  - `issue update <number> --status …` 等 — status / priority などの更新（CLI / MCP と同じ usecase を呼ぶ）。
  - 引数なしは一覧（あるいは issue 一覧モーダル / 専用ビュー）を開く。
- **関連性テーブル**: `dependson` を列に持つ一覧を描画し、依存先が未完了の issue（着手ブロック中）を
  視覚的に区別する（色分け・マーカー）。可能なら被依存（このissueをブロックしている先）も示す。
- **進捗の可視化**: status 別の件数集計と完了率（done / 全体）をヘッダーや進捗バーで表示する。
  ラベルやマイルストーン単位での集計も検討する。
- usecase は 023〜025 で確立済みのものを再利用し、presentation（TUI）層に閉じて実装する。
- 一覧の整形・進捗集計・依存解決などの純粋ロジックはテスト可能な関数に切り出し、カバレッジ 100% を維持する。
- `document/03-commands/02-tui.md` と `document/design/05-home.md` に `issue` コマンド・ビューを追記する。

## 完了条件

- ホーム画面のコマンドモードで `issue` を実行でき、issue 一覧がテーブル表示される。
- テーブルに各 issue の `dependson` が表示され、依存未完了でブロックされている issue を判別できる。
- status 別の集計・完了率（進捗）が画面上で確認できる。
- 一覧・進捗・依存解決のロジックにテストが追加され、カバレッジ 100% を満たす。

## 関連

- issue の永続化基盤は [023-issue-store](023-issue-store.md)、CLI は [024-issue-cli](024-issue-cli.md)、
  MCP 公開は [025-issue-mcp](025-issue-mcp.md)。本 issue はそれらの usecase を TUI から再利用する。
- 詳細表示の Markdown レンダリングは [033-preview-viewer](033-preview-viewer.md) と共有できる。
- コマンドレジストリ（拡張点）は [document/design/05-home.md](../document/design/05-home.md) を参照。
