---
number: 755
title: feat(workflow): 計画の実施を証拠で確かめる
status: todo
priority: medium
labels: [v2, daemon, workflow, agent]
dependson: []
related: [738]
created_at: 2026-09-13T00:00:00+00:00
updated_at: 2026-09-13T00:00:00+00:00
---

## 問題

workflow は**レビューには証拠を要求する**（認証済み peer message、exact binding、request ID と commit SHA の
一致）が、**計画には何も要求しない**。`Phase` に計画の工程は無く、`bind` は直接 `Implementing` に入る。
計画担当を起動するようにという指示は起動 prompt の英文だけにあり、実装担当がそれを無視しても workflow から
見れば正常である。

結果として「レビューは検証、計画は信仰」という非対称が残り、#738 の watchdog を入れても計画の不在は
検出できない（証拠が無いので、そもそも何を待てばよいか決まらない）。

## 方針

- 計画の完了を、レビューと同じ「観測可能な証拠」で定義する。候補は、実装担当が計画担当へ handoff した
  binding と、計画担当から実装担当への返答（`agent_message`）の観測である。
- 証拠が揃うまでの工程（例: `Planning`）を持つか、`Implementing` のまま「計画未確認」を投影するかを設計で決める。
  phase を増やす場合は、既存 record の互換（`serde` 既定）と TUI の表示を同時に扱う。
- 計画を必須にするか任意にするかも決める。省略できる小さな goal で強制すると摩擦になるため、
  「計画担当を指定した run では必須」が妥当と思われる。
- 計画の不在は失敗ではなく**待ち**として表現し、理由を人に見せる。

## 受入条件

- [ ] 計画の完了が観測可能な証拠で判定され、prompt の文面に依存しない。
- [ ] 計画を待っている run が、その理由とともに人に見える。
- [ ] 既存の record・TUI 表示と互換である。
- [ ] レビューの証拠判定（binding と ID の一致）と同じ厳しさである。
