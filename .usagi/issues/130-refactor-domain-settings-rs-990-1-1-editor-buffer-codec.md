---
number: 130
title: refactor(domain): settings.rs（~990 行）を 1 トピック 1 ファイルに分割し editor-buffer codec の層配置を見直す
status: todo
priority: medium
labels: [refactor, core, review]
dependson: []
related: []
created_at: 2026-07-04T23:17:16.803944+00:00
updated_at: 2026-07-04T23:17:16.803944+00:00
---

## 背景（なぜ問題か）

`domain/settings.rs` は実コードだけで ~990 行（テスト除く）あり、`Theme`/`AgentCli`/`KeyScheme`/`Sidebar`/`SessionActionUi`/`LabelColor`/`SkillFeature` 各 enum、`LocalLlm`、`SessionLabelDef`/`SessionLabelMaster`、`Settings`/`LocalSettings`、env バインディング検証、そしてラベル/env のエディタバッファ codec（`parse_env_bindings`/`format_env_bindings`/`parse_session_labels`/`format_session_labels` ＋ `LABEL_FIELD_SEP`）が 1 ファイルに同居している。規約「1 ファイル 1 トピック／300 行超は分割検討」に反する。

加えて `parse_session_labels` 等の「エディタバッファ ↔ Settings 型」変換は config 画面・コマンドパレット（presentation）が消費する UI フォーマットで、純関数とはいえ domain に置く必然性は薄い。

## 対象箇所

`src/domain/settings.rs`（ファイル全体、特に末尾の editor-buffer codec 群）

## やること

- `settings/` サブモジュール化する（例: `settings/labels.rs`＝LabelColor/SessionLabelDef/Master、`settings/agent.rs`＝AgentCli、`settings/env.rs`＝SecretEnv 検証、`settings/mod.rs`＝Settings/LocalSettings）。
- エディタバッファ codec は presentation 寄りのモジュール（または独立モジュール）へ寄せられるか検討する。

## 受け入れ条件

- 各サブファイルが 300 行以内で 1 トピックに収まり、公開 API（`use crate::domain::settings::…`）は据え置き。
- 既存テストが緑、カバレッジ 100% 維持。
