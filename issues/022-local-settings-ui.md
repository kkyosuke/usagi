---
number: 022
feature: local-settings-ui
title: ローカル設定の編集 UI
status: todo
priority: medium
category: tui
dependson: [021]
ref: PR #33（ローカル設定のバックエンド実装）
---

# ローカル設定の編集 UI

## 概要

プロジェクト単位のローカル設定（`<repo>/.usagi/settings.json`）の **読み書きロジック・永続化は #021 で実装済み**（`domain::settings::LocalSettings` / `usecase::settings` の `load_local` / `save_local` / `effective` / `set_local_*`）ですが、それを **編集する UI が未実装**です。現状ユーザーが値を変えるには JSON を直接編集するしかありません。本 issue でローカル設定を編集できる導線を追加します。

対象項目は `agent_cli` と `notifications_enabled`（未設定ならグローバル設定にフォールバック）。詳細は [document/data-storage.md](../document/data-storage.md) の「`settings.json`: プロジェクト固有の設定上書き」を参照。

## やること

- TUI Config 画面に、各項目を **「グローバルに従う / ローカルで上書き」** で切り替えられる UI を追加する。
  - 上書き ON のときだけ値（claude/gemini、通知 ON/OFF）を選択できる。
  - 現在の実効値（グローバル + ローカル上書きの結果）が分かる表示にする。
- 保存時に `usecase::settings::set_local_*`（または `save_local`）を呼ぶ。全項目が未設定になった場合の `settings.json` の扱い（空ファイルを残す / 削除する）を決める。
- 必要に応じて CLI（例: `usagi config --local`）からも確認・編集できるようにする。

## 完了条件

- TUI から各プロジェクトのローカル設定を上書き／解除でき、`<repo>/.usagi/settings.json` に反映される。
- 未設定の項目はグローバル設定にフォールバックして表示・動作する。
- 実効設定（`effective`）が TUI / 通知などの実際の挙動に反映される。
