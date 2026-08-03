---
number: 628
title: ci(v1): 出荷バイナリの coverage 100% gate を復旧する
status: in-progress
priority: medium
labels: [review, v1, ci, test, coverage, release]
dependson: []
related: [179, 380, 484]
created_at: 2026-08-02T23:11:04.057216+00:00
updated_at: 2026-08-03T01:17:43.718126+00:00
---

## Finding（P2 / shipping gate と仕様の drift）

### path / symbol

- `.github/workflows/v1-test.yml::jobs.v1-test`
- `.github/workflows/coverage.yml::jobs.coverage-run`
- `scripts/coverage.sh::{coverage_enforce, coverage_report}`
- `v1/document/02-architecture.md::テスト方針`
- `v1/document/06-conventions.md::品質チェック`
- `v1/README.md::Coverage badge`

### 発生条件

現在のリリース起点である `v1/**` の production branch/error pathを変更してPRを作る。`v1-test.yml` は stable の fmt/clippy/`cargo test` だけを実行し、root `coverage.yml` / `scripts/coverage.sh` はv2 workspaceだけを計測する。

### 影響

現在配布されるv1 binaryに未テストbranchを追加しても、line/function coverage 100% gateは実行されずPRがgreenになり得る。それにもかかわらずv1の設計・規約はCI coverage 100%を保証すると記載し、v1 READMEのCoverage badgeはv2だけを測るworkflowを表示するため、利用者・reviewerはshipping artifactの品質保証を誤認する。

### 具体的根拠

- `document/06-conventions.md#リリース` と `.github/workflows/v1-test.yml` は、現在の出荷物が `v1/Cargo.toml` からbuildされると明記する。
- `v1-test.yml` に `cargo llvm-cov`、threshold、coverage reportのstepは無い。
- root `Cargo.toml` は `v1` をworkspaceからexcludeし、`scripts/coverage.sh` もv2 packagesだけを選ぶ。
- `v1/document/02-architecture.md` は「CIでカバレッジ100%」、`v1/document/06-conventions.md` は `.github/workflows/coverage.yml` が100%を実行すると断言するが、実workflowはv1を計測しない。
- `v1/README.md` のCoverage badgeはroot `coverage.yml` であり、shipping v1のcoverage状態ではない。
- 履歴の #774（commit `3d8a745d`）はv1差分で無関係なv2 coverageを起動しないための変更であり、v1専用coverage gateを不要とする根拠にはならない。

### 修正方針

v1独立manifestを対象にしたcoverage command/threshold/exclusion policyをSSoT化し、`v1/**` Rust差分のPRでv1専用coverage jobを必須実行する。v2 workflowを誤ってv1へ適用せず、release artifactと同じsource treeを計測する。badgeとv1 archived docsは実際のgateへ同期する。

### 必須回帰テスト

- v1 sourceの未到達line/functionをfixture changeで作るとv1 coverage aggregateが失敗する。
- 100%時は成功し、v2-only/docs-only PRでは重いv1計測をskipしてstable aggregateを返す。
- v1の許可済みreal IO exclusionだけを明示し、business/parser/error pathをfilename一括除外しない。
- v1 full test、release build check、root v2 coverageの既存gateを維持する。
- badgeがv1 coverage workflowの結果を表示する。

### docs / migration

v1の挙動を変更するため、archiveを新機能仕様として編集するのではなく、shipping verification contractのdrift修正として `v1/document/02-architecture.md`、`v1/document/06-conventions.md`、`v1/README.md` を実workflowと同期する。runtime/data migrationはない。
