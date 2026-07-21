---
number: 504
title: feat(daemon): Codex structured capture channel を配線して resume を有効化する
status: done
priority: high
labels: [daemon, agent, recovery]
dependson: [503]
related: [350, 388, 390]
created_at: 2026-07-21T12:00:00+00:00
updated_at: 2026-07-21T22:42:31.293558+00:00
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

## 調査結果と採用 channel

2026-07-22 時点の公式 Codex manual の [Hooks](https://learn.chatgpt.com/docs/hooks) を確認した。
command hook は event ごとに JSON object を stdin で受け取り、全 event 共通 field の
`session_id` は「current Codex session id」、`hook_event_name` は current event name と
documented されている。また `SessionStart` は thread start scope の event で、matcher の
start source に `startup` / `resume` / `clear` / `compact` を持つ。

採用する production channel は `SessionStart` command hook の `startup` event である。

```text
Codex SessionStart(startup) JSON stdin
  -> hidden `usagi codex-session-capture`
  -> daemon-minted runtime credential を添えた private IPC
  -> credential から exact live Codex runtime を解決
  -> capture_structured_provider_session
  -> ProviderCaptureProvenance::ProviderStructured
```

adapter は one-shot config で hooks を有効化し、`SessionStart` matcher を `^startup$` に限定する。
明示 resume は保存・再検証済み ID を既に持つため、この capture hook の対象にせず
`codex resume <SESSION_ID>` を使う。hook input に含まれ得る `transcript_path` は
deserialize せず、file を開かない。

### provider 互換性条件

数値 version を推測で固定せず、次の documented capability をすべて持つ Codex CLI を互換とする。

- lifecycle hooks と `SessionStart` command event を解釈する。
- command hook の stdin JSON に string の `session_id` と `hook_event_name = "SessionStart"` を渡す。
- `startup` matcher と、daemon が指定する `--dangerously-bypass-hook-trust` を受理する。
- one-shot config の `features.hooks` / `hooks.SessionStart` を受理する。

ローカル調査環境の `codex-cli 0.144.1` は上記 config shape を strict-config 初期化段階まで
受理した。managed policy による hooks 無効化、非対応 CLI、hook skip / timeout / non-zero exit、
malformed JSON、空・option-like・制御文字を含む ID、credential 欠落/失効、daemon/persistence failure
では metadata を記録しない。version/name/path/transcript/state database からの fallback は行わず、
`provider_metadata_unavailable` のまま fail-closed にする。

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
