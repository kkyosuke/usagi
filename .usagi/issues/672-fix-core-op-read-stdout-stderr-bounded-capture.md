---
number: 672
title: fix(core): op read の stdout / stderr を bounded capture にする
status: done
priority: high
labels: [review, v2, core, env, process, resource, security]
dependson: []
related: [606, 661]
created_at: 2026-08-13T00:05:12.227584+00:00
updated_at: 2026-08-17T00:43:30.331123+00:00
---

## Finding（P1 resource / availability）

`crates/core/src/infrastructure/env_resolver.rs` は binding 数 128、secret reference 数 32、同時 `op read` 数 4、1 child 30 秒の deadline を持つ。一方、実 child の stdout / stderr reader はそれぞれ `read_to_end(&mut Vec::new())` であり byte 上限がない。

PATH 上の `op` が壊れている、wrapper に置換されている、または大量の診断を出すと、最大 4 child × 2 stream が deadline まで無制限に heap を確保する。timeout → terminate/kill → reap と reader join があっても、その前に保持した output bytes は bounded ではない。

## 修正方針

- stdout / stderr ごとに 64 KiB の hard capture limit を置く。
- limit 到達後も pipe は EOF まで drain し、child を pipe backpressure で停止させない。保持する `Vec` だけを hard cap 内に保つ。
- どちらかの stream が超過した正常/非正常終了は、raw output や secret を含まない stable failure にする。
- cancellation / timeout でも従来どおり exact child の cleanup、wait/reap、**stdout / stderr両readerのjoin**を完了し、output memory boundを破らない。
- 片方のreaderがpanicまたはread errorになっても、先にerror returnしてもう片方をdetachしない。両handleを消費・joinしてからdeterministicなfailureを返す。
- [document/09-env.md](../../document/09-env.md) に現在の output bound と超過時の binding failure を記載する。

## 受入条件

- [x] stdout が上限ちょうどなら成功し、上限 + 1 byte なら safe failure になる。
- [x] stderr が上限を超える nonzero child も retained bytes が上限内で、raw stderr を返さない。
- [x] limit 到達後に残りを drain し、EOF まで読んだことを fake reader で固定する。
- [x] timeout/cancellation は cleanup と join を維持し、巨大 output を理由に別の unbounded allocationを作らない。
- [x] stdout / stderrの片方が失敗しても両readerをjoinし、sibling readerをdetachしない。
- [x] literal / secret 成功 / 個別 failure / service-account token redaction の既存契約を変えない。
- [x] core selected tests、fmt/check/clippy、workspace full test、Markdown link check を通す。coverage 100% は PR CI で確認する。

## 非対象

- `op` 以外の subprocess policy。
- secret value 自体の保存や IPC 配送（引き続き行わない）。

## 検証

- `cargo build --workspace`
- `cargo fmt --all -- --check`
- `cargo check --workspace --all-targets`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo test -p usagi-core`（920 unit + 6 integration + 1 doc）
- `cargo test -p usagi-core infrastructure::env_resolver`（19 tests）
- `cargo test --workspace --quiet`
- `ruby scripts/coverage-off-lint.rb`（228 exclusions。#689 の retirement poll 追加後の最新 main）
- `scripts/recommend-tests.sh --validate-map`
- `bash scripts/tests/recommend-tests.sh`
- `cargo audit`（145 dependencies / vulnerabilities 0）
- `lychee --config lychee.toml --no-progress ...`（1803 total / 0 errors）
