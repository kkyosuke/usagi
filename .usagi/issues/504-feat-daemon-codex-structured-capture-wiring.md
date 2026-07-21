---
number: 504
title: feat(daemon): Codex structured capture channel を配線して resume を有効化する
status: todo
priority: high
labels: [daemon, agent, recovery]
dependson: [503]
related: [350, 388, 390]
created_at: 2026-07-21T12:00:00+00:00
updated_at: 2026-07-21T12:00:00+00:00
---

## 背景

#503 で daemon は `capture_structured_provider_session`（structured capture 境界）を持ち、
provider が公開する正式な構造化 channel から届いた Codex session ID だけを
`ProviderResumeRef` として永続化できる。ただし現在のビルドには、この境界へ ID を供給する
production の capture 経路が存在しない。そのため Codex runtime は設計どおり fail-closed で
常に resume 不可（`provider_metadata_unavailable`）になる。Claude は daemon 発行 UUID で
resume 可能であり、本 issue は Codex 側の capture 供給だけを対象にする。

## 目的

Codex CLI が公開する正式な構造化経路（documented な API / event / command result）から
runtime-bound な session ID を adapter 境界で受け取り、`capture_structured_provider_session`
へ配線して Codex の明示 resume を有効化する。

## 非目標（#503 の禁止事項を維持する）

- transcript、state database、設定、履歴ファイルの検出・走査・parse による ID 取得。
- `--last` への downgrade、空 ID・推測 ID の受理。
- 正式な構造化経路が存在しない・失敗した場合の fallback（fail-closed を維持する）。

## 受入条件

- Codex の正式な構造化 channel を調査し、採用する経路（例: structured output mode、
  session-created event、command result）と provider の互換性条件を issue に記録する。
- 採用した経路から得た ID だけが `capture_structured_provider_session` に届き、
  `ProviderCaptureProvenance::ProviderStructured` で永続化されることを fixture で検証する。
- capture 経路の失敗・不在時に resume 不可のままであること（fail-closed）を fixture で固定する。
- `document/05-daemon.md` の「現在のビルドは capture 経路を持たない」旨の記述を実装に合わせて更新する。
- full coverage を維持する。
