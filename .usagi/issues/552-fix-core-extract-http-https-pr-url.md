---
number: 552
title: fix(core): extract が後続の http:// により先行する https:// PR URL を取りこぼす
status: todo
priority: high
labels: [review, v2, core, daemon, pullrequest, correctness]
dependson: []
related: [518]
created_at: 2026-07-25T22:56:54.411474+00:00
updated_at: 2026-07-25T22:56:54.411474+00:00
---

## 問題・根拠（コード調査で確定）

`crates/core/src/domain/pr_inventory.rs` の `extract` は、1 回のスキャンで次の 2 段構えの探索を行う。

```rust
let Some(relative) = bytes[start..]
    .windows(7)
    .position(|window| window == b"http://")
    .or_else(|| {
        bytes[start..]
            .windows(8)
            .position(|window| window == b"https://")
    })
else { break };
```

`http://` の探索が**バッファ全体**に対して先に走り、見つかればその位置へ `start` を進めてしまう。`"https://"` は `"http://"` を部分文字列として含まない（`https:/` ≠ `http://`）ため、**`http://` の出現が後方にあると、その手前にある `https://` の PR URL がすべて読み飛ばされる**。

`.or_else` は「`http://` が 1 つも無いときだけ `https://` を探す」という意味になっており、両方が混在する入力で誤った候補を選ぶ。

### 実測（この worktree の HEAD で確認）

`crates/core` の `extract` を直接呼んで確認した。

| 入力 | `extract` の戻り値 |
|---|---|
| `https://github.com/o/r/pull/1 and nothing else` | `["https://github.com/o/r/pull/1"]` |
| `https://github.com/o/r/pull/1 and http://example.com/x` | `[]` |

後者は PR URL がそのまま出力に現れているのに、無関係な `http://` リンクが後続するだけで検出が 0 件になる。

### 影響

`extract` の唯一の呼び出し元は `crates/daemon/src/usecase/pr_inventory.rs` の
`OutputPrProjector::observe_committed` であり、daemon は committed PTY output からこの関数だけで PR を検出する。
agent の出力は PR URL と併せて docs / issue / CI の URL（`http://localhost:...` を含む）を吐くことが普通にあるため、
この取りこぼしは実運用で踏まれる。取りこぼした PR は `pr-inventory.json` に入らないので、TUI / MCP
（[document/07-mcp.md](../../document/07-mcp.md) の `session_pr`）にも一切現れず、利用者から見ると「PR を出したのに
usagi が気づかない」という無言の欠落になる。

[document/02-architecture.md](../../document/02-architecture.md) は「daemon は journal に commit 済みの PTY output から
HTTP(S) の `github.com/<owner>/<repo>/pull/<number>` だけを検出し」と書いており、現状の実装はこの記載を満たしていない。

## 既存 issue との境界

- [#493](493-fix-daemon-pr-refreshscheduler-production-worker.md)（done）は検出済み identity の **refresh scheduling** が対象で、抽出そのものは対象外だった。
- [#518](518-refactor-daemon-owner-generation-runtime-shard-global-resource-allocator.md) は `pr-inventory.json` の cross-generation single writer を扱う。**どの URL を検出するか**は対象外である。
- 本 issue は `extract` の探索順の 1 か所に限定する。hot path 上のコスト（重複再スキャン・per-chunk の disk read）は
  別 issue（daemon 出力 hot path の corrective optimization）が扱う。**本 issue は Vec の割り当てやスキャン回数を最適化しない**。

## やること

- `http://` と `https://` を**同一のスキャンで最も早い出現位置**として扱うようにする（`or_else` による全体 2 段探索をやめる）。
- 上表の 2 ケースを固定する unit test を `crates/core/src/domain/pr_inventory.rs` に追加する。

## 設計上の判断が必要な点

- **`https://` を `http://` より優先するか**。`"https://x"` の位置 0 は `https://` に一致し、`http://` には一致しないので競合しないが、
  `"http://https://..."` のような病的入力での候補境界の決め方（どちらの scheme から候補を切り出すか）を明示しておく。
- **候補の終端判定は変えない**。`extract` は空白・制御文字・`'"<>` で終端し、末尾の約物を削る現行の規則がそのまま
  canonicalize の入力になる。ここを触ると `canonicalize` の fail-closed 判定（credential・control character・不正 percent
  encoding・非 GitHub host・0/overflow の番号）と二重に効いてしまう。

## 受入条件

- [ ] `https://…/pull/<n>` の後方に `http://` を含む入力で、PR identity が検出される。
- [ ] 逆順（`http://` が先、`https://` の PR が後）でも検出される。
- [ ] 複数の PR URL と複数の非 GitHub URL が混在する入力で、重複なく全 PR が canonical URL として検出される。
- [ ] 既存の `extraction_trims_punctuation_and_deduplicates` を含む既存 test が変わらず通る。
- [ ] カバレッジ 100% を維持する。
- [ ] `document/` の更新は不要か、必要なら [document/02-architecture.md](../../document/02-architecture.md) の検出記載と一致させる（記載＝実装済みを守る)。

## 必須回帰テスト・計測

- `cargo test -p usagi-core`（`domain::pr_inventory` の unit test）
- `cargo test -p usagi-daemon`（`OutputPrProjector` の投影 test が退行しないこと）
- 計測は不要。本 issue は探索順の修正のみで、スキャンの計算量を変えない。
