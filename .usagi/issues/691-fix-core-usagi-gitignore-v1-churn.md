---
number: 691
title: fix(core): .usagi/.gitignore の行順を v1 と揃えて交互起動の churn を止める
status: todo
priority: medium
labels: [core]
dependson: []
related: [690]
created_at: 2026-08-17T22:49:19.551382+00:00
updated_at: 2026-08-17T22:49:19.551382+00:00
---

設計は [document/proposals/16-v1-v2-coexistence.md](../../document/proposals/16-v1-v2-coexistence.md) の
「設計 2: `.usagi/.gitignore` の行順を v1 に揃える」が正本。

## 背景

v1 と v2 の `USAGI_GITIGNORE` は `.lock` と `.derived-dirty` の**行順だけ**が異なる。

```
v1: /issues/index.json  /issues/.lock          /issues/.derived-dirty
v2: /issues/index.json  /issues/.derived-dirty /issues/.lock
```

どちらの writer も「内容が完全一致しなければ書く」idempotent 実装なので、同じ workspace を
v1 で開き v2 で開くと **tracked file が毎回 dirty になる**。`.usagi/.gitignore` は
`!/.gitignore` で追跡対象なので、session の PR に無関係な差分が混ざる。

意味は同じで、揃える側は v2 である（v1 は出荷物なので変更しない）。

## やること

- `crates/core/src/infrastructure/gitignore.rs` の `USAGI_GITIGNORE` の 2 行を入れ替え、
  v1 の `v1/src/infrastructure/gitignore.rs` の定数と byte 一致させる。`issues` / `memory` の
  両ブロックが対象。
- v2 の test から v1 の定数は参照できない（`v1/` は workspace から exclude）。期待値を v2 側の
  test に literal として置き、**v1 と byte 一致させる意図**を doc comment に書く。行を並べ替えた
  だけで churn が再発するので、期待値 literal は 1 か所に閉じる。
- 既存の `<repo>/.usagi/.gitignore` は現在 v2 の順序で commit されている。この変更をマージすると
  v1 順序へ書き換わるため、**同じ PR で `.usagi/.gitignore` を新しい順序へ更新**して、以後どちらの
  binary で開いても dirty にならない状態にする。

## テスト

`cargo test -p usagi-core`: 定数の byte 一致、既存内容が一致するとき書かないこと、
異なるとき書くこと（`gitignore.rs` の既存 test を更新）。
