---
number: 766
title: chore: nursery lint（redundant_clone / cognitive_complexity）の指摘を返済し allow を棚卸しする
status: todo
priority: low
labels: [v2, refactor, lint]
dependson: []
related: [758, 760, 761]
created_at: 2026-09-16T00:00:00+00:00
updated_at: 2026-09-16T00:00:00+00:00
---

## 問題

workspace は `clippy::all` + `clippy::pedantic` を warn にしているが、nursery の 2 lint を追加で回すと次の件数が出る
（2026-09-16、`cargo clippy --workspace --all-targets -- -W clippy::redundant_clone -W clippy::cognitive_complexity`）。

| lint | 件数 | 内訳 |
|---|---|---|
| `redundant_clone` | 252 | tui 87 / daemon 63 / core 50 / src 45 / cli 2 / tests 5 |
| `cognitive_complexity`（閾値 25） | 39 | production 18（tui 6 / daemon 10 / src 2）、残りは test |

`redundant_clone` の上位は `src/runtime/daemon.rs` 30 件、`crates/daemon/src/usecase/agent_ipc.rs` 20 件、
`crates/core/src/infrastructure/client.rs` 15 件である。

また pedantic を通すための `#[allow(clippy::…)]` は production コードに約 80 件あり、`too_many_arguments` 27 件、
`too_many_lines` 27 件が大半を占める。多くは #757 / #758 / #760 / #761 の分割で不要になる。

## 方針

- `redundant_clone` は機械的に除去する（挙動不変。1 crate ずつ PR を分ける）。
- `cognitive_complexity` の production 18 件は該当 issue の分割で解消し、残る箇所だけ個別に判断する。
- 分割完了後に `#[allow(clippy::too_many_lines)]` / `too_many_arguments` を棚卸しし、残す allow には理由コメントを
  必須にする。理由付きで残す運用を `document/06-conventions.md` に 1 行追記する。

## 完了条件

- `redundant_clone` 0 件。
- production の `cognitive_complexity` 25 超が 5 件以下。
- 理由コメントの無い `#[allow(clippy::…)]` が production コードに無い。
