---
number: 113
title: refactor: issue/memory のドメイン〜インフラにわたる二重実装を横断的に解消する（親）
status: done
priority: medium
labels: [refactor, review]
dependson: []
related: []
created_at: 2026-07-04T23:12:50.196962+00:00
updated_at: 2026-07-04T23:12:50.196962+00:00
---

## 背景（なぜ問題か）

`Issue` と `Memory` は「frontmatter 付き markdown を SoT、`index.json` を派生キャッシュ」というほぼ同一のデータモデルを持ち、**domain / usecase / infrastructure の 3 層すべてで並行して二重実装**されている。全文検索プリミティブ（`usecase/search.rs`）と frontmatter 原始関数（`domain/frontmatter.rs`）は既に共通化済みだが、その外側の「封筒処理・CRUD オーケストレーション・ストア永続化・JSON ビュー」が対になってコピペされている。

このため、どちらか一方に機能追加・仕様変更を入れるたびに両方を揃えて直す必要があり、実際に **memory 側だけキャッシュ鮮度判定（`load_fresh_index`）が欠落**して外部編集後に stale を返しうるという挙動ドリフトが既に発生している。

## スコープ

この親 issue は横断リファクタの傘であり、実装は層ごとに切った子 issue で行う。子 issue:

- refactor(domain): issue/memory の markdown 封筒処理を frontmatter に集約する
- refactor(domain): issue/memory の enum 文字列トリオ・ParseError・Summary・JSON view の重複を集約する
- refactor(usecase): issue/memory の store-backed CRUD を共通化する
- refactor(infra): issue_store/memory_store の markdown+index 永続化を MarkdownStore に共通化する（memory の鮮度判定欠落も是正）

## やること

- 上記子 issue を順に実装する（domain の封筒 → usecase CRUD、infra ストアは並行可）。
- 各層で「issue 固有・memory 固有」の差分（readiness 注釈・`sort_newest_first`・番号採番 vs slug キー・TOC 有無・`#[serde(rename="type")]` など）はフック/型パラメータとして残し、共通骨格だけを一本化する。

## 受け入れ条件

- 全子 issue が done。
- issue/memory 双方の markdown 読み書き・CRUD・ストア永続化・JSON ビューの共通骨格が単一実装に集約され、各エンティティ側にはフィールド差・後処理フックだけが残る。
- 既存テストが全緑、カバレッジ 100% 維持。
