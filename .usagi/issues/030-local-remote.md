---
number: 30
title: セッション開始時のデフォルトブランチ基点（local / remote）
status: done
priority: medium
labels: [tui]
dependson: [22]
related: []
created_at: 2026-06-16T23:05:45.185106+00:00
updated_at: 2026-06-16T23:08:31.717780+00:00
---

# セッション開始時のデフォルトブランチ基点（local / remote）

## 概要

`session new <name>` でセッションを作ると、各 git リポジトリの worktree は新しい `<name>` ブランチを切って作成されます。従来この**基点**は常にそのリポジトリの現在の HEAD でしたが、選ぶ手段がありませんでした。

本 issue では、新ブランチの基点を **`local`（ローカルの既定ブランチ）/ `remote`（リモート追従の既定ブランチ）** から選べるようにし、**リポジトリ単位のローカル設定**（`<repo>/.usagi/settings.json` の `default_branch_source`）として保存します。複数 git を含むワークスペースでは各リポジトリ内で Config を開いて個別に設定します。

詳細仕様は [document/05-settings.md](../document/05-settings.md#ローカル設定プロジェクト単位の上書き) / [document/data/02-workspace.md](../document/data/02-workspace.md#settingsjson-プロジェクト固有の設定上書きローカル設定) / [document/04-orchestration.md](../document/04-orchestration.md) を参照。

## やること

- `domain::settings` に `BranchSource { Local, Remote }`（既定 `Remote`）を追加し、`LocalSettings` に
  `default_branch_source: Option<BranchSource>` を持たせる。
- `infrastructure::git` に基点解決 `resolve_base_ref(repo, source)` を追加し、`add_worktree` に基点
  （`Option<&str>`）を渡せるようにする。`remote` は `origin/<既定>` → ローカル `<既定>` → 現 HEAD の順で
  フォールバックする。
- `usecase::session::create` / `build_dir` で、各リポジトリのローカル設定から基点を解決して worktree を切る。
- TUI Config 画面のローカル設定に「Local · Default Branch」行（`local` / `remote` トグル、未設定時は
  `Default (Remote)`）を追加する。

## 完了条件

- リポジトリのローカル設定で `local` / `remote` を選べ、`<repo>/.usagi/settings.json` に
  `default_branch_source` として保存される。
- `session new` 時、各リポジトリの設定どおりの基点（ローカル既定ブランチ / `origin/<既定>`）から worktree が
  切られる。リモートが無い等で基点が解決できないときは現在の HEAD にフォールバックする。
- 複数 git を含むワークスペースで、リポジトリごとに異なる基点を適用できる。
- 既存のテスト・カバレッジ（100%）を維持する。
