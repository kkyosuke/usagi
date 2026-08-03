---
number: 639
title: fix(v1): Closeup の agent で -m / --model を受け付ける
status: in-progress
priority: medium
labels: []
dependson: []
related: [545]
created_at: 2026-08-03T21:22:53.633018+00:00
updated_at: 2026-08-03T21:23:17.287043+00:00
---

## 背景

出荷バイナリ（`v1/Cargo.toml` 起点。現行 2.9.1）の Closeup `agent` は位置引数の名前しか受け付けず、
`agent -m claude` と打つと `unknown agent "-m claude" (try agent [name])` で拒否される。

一方、v2（root workspace）の Closeup は #545 で `agent [-m|--model <cli>]` を実装済みで、
`-m` が正規の書き方になっている。ユーザーが実際に触るのは v1 の出荷バイナリなので、
**同じ入力が build によって通ったり落ちたりする**状態になっていた。

## 目的

v1 の Closeup `agent` でも `-m` / `--model` を受け付け、v2 と同じ入力が通るようにする。
位置引数の形（`agent codex`）はこれまでの書き方なのでそのまま残す。

## 変更内容

- v1 `presentation/tui/home/command/builtins.rs`:
  - `agent [[-m|--model] <name>]` を解釈する `parse_agent_selection` を追加。
    `-m` / `--model` と裸の名前は同じ `AgentCli::from_name` の語彙を通る。
  - 拒否は安全な 1 行のエラーで返す（未知の名前 / 未知のフラグ / `-m` の値欠落 / 2 つ以上の指定）。
    起動 effect は出さない。
  - `usage` を `agent [-m <name>]` に、examples に `agent -m claude` を追加。
  - Tab 補完を 3 token 文法へ拡張（`agent ` は `-m` / `--model` ＋ CLI 名、`agent -m ` は CLI 名だけ）。
- v1 `document/03-commands/02-tui.md`: `agent` の引数と補完の記述を実装に合わせる。

## テスト・確認方法

- `cargo test --workspace --quiet`（v1 workspace）: `agent -m` / `--model` の解決、拒否文言、補完の 3 arm。
- `cargo fmt --all -- --check` / `cargo clippy --workspace --all-targets -- -D warnings`（v1 workspace）
- v1 coverage 100%（`scripts/v1-coverage.sh`）
- Markdown link check（lychee）
