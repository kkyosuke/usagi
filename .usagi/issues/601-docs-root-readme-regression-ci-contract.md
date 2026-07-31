---
number: 601
title: docs: root README の regression を復旧し CI で contract を守る
status: done
priority: high
labels: [docs, ci]
dependson: []
related: []
created_at: 2026-07-31T12:29:59.682732+00:00
updated_at: 2026-07-31T12:36:42.067665+00:00
---

## 背景

全体コードレビューで P1 の docs regression を発見した。リポジトリルートの `README.md` が
`fixture` の 1 行に破壊されたまま `main` に残っている。

- 原因コミット: `4db8595981e17cbce279890a660f908746ea5b9c`（commit message は `fixture`）
- 混入経路: PR #1026 `feat(core): supervisor durable state machine を追加`
  （squash merge `3f0c3e29`、2026-07-18）。README.md の 63 行が `fixture` 1 行へ置換された
  accidental fixture commit で、PR の本題（supervisor durable state machine）とは無関係。
- 残存期間: 2026-07-18 〜 2026-07-31（13 日間）。以降 README.md を触ったコミットは無い。

## なぜ誰も気づかなかったか

ルート README は、どの gate の検証対象にもなっていない。

| gate | 1 行 `fixture` README に対する挙動 |
|---|---|
| `test.yml`（fmt / clippy / full test） | Rust だけを見るため無関係 |
| `coverage.yml` | 同上 |
| `markdown-link-check.yml`（lychee） | **通る**。リンクが 0 本になるとリンク切れも 0 件になるため、README を空にする regression はリンクチェックでは検出できない |
| `scripts/recommend-tests.sh` | `*.md` は docs 扱いで lychee を推奨するだけ |

つまり「README の中身が消える」という壊れ方は、既存 CI が構造的に検出できない。

## やること

1. 事故直前（`4db8595^`）の README を復元する。
2. 復元内容を現在の `document/01-overview.md` / `02-architecture.md` / `06-conventions.md` と
   照合し、drift を必要最小限で直す。
3. 再発防止として、ルート README が最低限の contract（`# usagi` 見出し・v2 architecture への
   リンク・v1 へのリンク・truncation 検出）を満たすことを検証する軽量 checker を追加し、
   `test.yml` の script tests に配線する。checker 本体は fixture test を持たせ、
   inline untested logic を CI に置かない。

## 完了条件

- `main` の README が v2 の実態を説明する内容に戻っている。
- README を空・1 行に破壊する変更が CI で落ちる。
- checker 自体が fixture test で検証されている。
