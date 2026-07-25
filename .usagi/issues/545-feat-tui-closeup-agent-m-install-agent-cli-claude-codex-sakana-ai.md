---
number: 545
title: feat(tui): closeup agent の -m で install 済み agent CLI（claude / codex / sakana.ai）を選択する
status: done
priority: medium
labels: []
dependson: []
related: []
created_at: 2026-07-25T01:27:29.671529+00:00
updated_at: 2026-07-25T01:54:13.752017+00:00
---

## 背景

v2 Closeup の `agent` は位置引数で profile 名を受け取るだけで、選択できる CLI の語彙・install 判定・config の
default とのつながりが無かった。config の `default_model`（`Settings`）も `profile_id()` が実際の起動へ配線されて
おらず、`agent` は daemon 側の hardcode default（`codex`）に委ねていた。

## 目的

Closeup の `agent` で起動する agent CLI を `-m` で選べるようにし、次を満たす。

- 選択肢は `claude` / `codex` / `sakana.ai`（Codex 互換 CLI。実行は `codex-fugu`、daemon profile は `sakana-ai`）。
- `-m` 省略時の default は config の `default_model`。
- Tab 補完（Prompt mode の入力欄と Action menu）が効く。
- **install 済みの CLI だけ**を表示・補完・受理する。未 install の CLI は候補に出さず、直接入力しても
  daemon へ request を送らずに安全な文言で拒否する。

## 変更内容

- core `domain/settings`: `DefaultModel` に `SakanaAi` を追加し、`ALL` / `profile_id` / `command` / `selector` /
  `from_selector` を SSoT 化。install 済み集合 `AvailableModels` を core へ移し、Config 画面と Closeup が共有する。
- tui `usecase/agent_command`: `agent [-m|--model <cli>]`（位置引数も可）の pure な parse / 補完候補 / picker 候補。
- tui `CloseupModal`: `agent` 行を `-m <cli>` の inline picker へ展開（default 行に `(default)` を表示）。
  Tab 補完を `agent -m <prefix>` の 3 token 文法へ拡張。
- tui controller: `AppState` に install 済み集合と default を注入し、`agent` submit を解決した profile ID 付きの
  `LaunchAgent` にする。notice はどの CLI を起動したかを表示する。
- daemon `CodexAdapter::sakana`: 同じ Codex argv 文法で program だけを `codex-fugu` にした `sakana-ai` profile。
  `provider_matches_profile` は `ProviderKind::Codex` に `sakana-ai` を許可する（resume 互換）。
- 合成ルート: 3 CLI を probe して TUI へ注入し、daemon registry に `sakana-ai` を登録する。
- `document/03-tui.md` に「Closeup の agent CLI 選択」を追加。

## テスト・確認方法

- `cargo test --workspace`（agent_command の文法、modal の picker / Tab 補完、controller の解決と拒否、
  daemon の `sakana-ai` plan、`provider_matches_profile`）
- `cargo clippy --workspace --all-targets -- -D warnings` / `cargo fmt --all -- --check`
- coverage 100%（`scripts/coverage.sh`）
- Markdown link check（lychee）
