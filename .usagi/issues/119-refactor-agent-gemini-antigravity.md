---
number: 119
title: refactor(agent): Gemini/Antigravity アダプタをパラメータ化して統合する
status: done
priority: medium
labels: [refactor, agent, review]
dependson: []
related: []
created_at: 2026-07-04T23:14:30.368423+00:00
updated_at: 2026-07-04T23:14:30.368423+00:00
---

## 背景（なぜ問題か）

`infrastructure/agent/gemini.rs` と `antigravity.rs` の `launch_command` / `headless_command` は約 80% 同一で、差分は 4 つだけ — program 名（`gemini`/`agy`）・resume フラグ（`-r latest`/`-c`）・headless bypass フラグ（`--yolo`/`--dangerously-skip-permissions`）・model フラグ（`-m`/`--model`）。`session_opening_prompt` の `-i=`/`-p` への差し込みや「wiring を inline しない」方針は完全一致している。`CodexAgent` が `codex`/`codex-fugu` を `program`/`home_subdir` でパラメータ化して 1 実装で賄っているのと同じ構図であり、同じ手法で吸収できる。

## 対象箇所

- `src/infrastructure/agent/gemini.rs`
- `src/infrastructure/agent/antigravity.rs`

## やること

- 上記 4 値を保持する「prompt 先頭・wiring 非 inline」の共通アダプタに統合する。
- resume/forget のセッション探索バックエンドは実装が異なる（`-r latest` vs `history.jsonl`）ので、分離のまま注入する。

## 受け入れ条件

- 両 CLI の生成コマンド文字列が現状と一致（テストで固定）しつつ、コマンド組み立てが 1 実装になる。
- 既存テストが緑、カバレッジ 100% 維持。

## 補足

#47（done、Agent アダプタの二重間接解消）とは別スコープ（あちらは trait 二重間接、こちらは gemini/antigravity の実装重複）。
