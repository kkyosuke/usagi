---
number: 021
feature: local-settings
title: プロジェクト単位のローカル設定（設定上書き）
status: done
priority: medium
category: core
dependson: []
ref: PR #33
---

# ローカル設定（プロジェクト単位の設定上書き）

## 概要

グローバル設定（`~/.usagi/settings.json`）の一部を、リポジトリごとに上書きできるローカル設定を追加します。保存先は `<repo>/.usagi/settings.json`。`.usagi/` は `.gitignore` 済みのため、コミットされずマシンローカルに保持されます。

対象項目は `agent_cli` と `notifications_enabled` の 2 つ。各項目は任意で、未設定（`null`）ならグローバル設定にフォールバックします。

## やったこと

- `domain::settings` に `LocalSettings` 型と、グローバルへローカルを適用する `Settings::with_local` を追加。
- `infrastructure::workspace_store::WorkspaceStore` に `settings.json` の `load_settings` / `save_settings` を追加（`state.json` と共通の atomic read/write ヘルパーに集約）。
- `usecase::settings` に `load_local` / `save_local` / `effective` / `set_local_agent_cli` / `set_local_notifications_enabled` を追加。
- `document/data-storage.md` にローカル設定の仕様を追記。

## 完了条件

- ローカル設定が `<repo>/.usagi/settings.json` に読み書きされる。✅
- 未設定の項目はグローバル設定にフォールバックする。✅
- 実効設定 = グローバル + ローカル上書き（`effective`）として解決できる。✅

> 編集 UI（TUI Config / CLI）は本 issue のスコープ外。[022-local-settings-ui](022-local-settings-ui.md) を参照。
