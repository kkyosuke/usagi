---
number: 627
title: fix(coverage): C locale でも exclusion linter を UTF-8 source に対応させる
status: done
priority: low
labels: [review, v2, test, coverage, tooling]
dependson: []
related: []
created_at: 2026-08-02T23:07:31.947688+00:00
updated_at: 2026-08-03T01:12:01.268298+00:00
---

## Finding（P3 / ローカル gate の再現可能な失敗）

### path / symbol

- `scripts/coverage-off-lint.rb::scan`
- `scripts/tests/coverage-off-lint.sh`

### 発生条件

UTF-8 日本語を含む現行 Rust source がある checkout で `LC_ALL=C LANG=C ruby scripts/coverage-off-lint.rb` を実行する。

### 影響

規約が coverage exclusion 変更時に必須とする policy lint が、policy 違反を報告する前に `ArgumentError: invalid byte sequence in US-ASCII` で異常終了する。managed session や minimal CI image が C locale の場合、開発者は registry の追加・削除・期限をローカルで検証できない。fixture suite はASCII sourceだけなのでこの環境依存回帰を検出しない。

### 具体的根拠

`scan` は `File.readlines(file, chomp: true)` を外部 encoding のまま読み、80行目の `line.match?(ATTRIBUTE)` で例外になる。現worktreeでは `LC_ALL=C` で同じ stack traceを再現した。一方 `bash scripts/tests/coverage-off-lint.sh` はASCII fixtureだけのため green になる。

### 修正方針

Rust sourceとJSON manifestを明示的にUTF-8として読み、invalid UTF-8は path/line を伴う controlled lint error に変換する。fixture runner自体を C locale で動かして外部 locale に依存しないことを固定する。

### 必須回帰テスト

- C locale + 日本語 comment/source + 有効な coverage metadata が成功する。
- C locale + invalid UTF-8 source が stack traceではなく決定的な lint errorになる。
- 既存 allowed/forbidden/stale/added/deleted/expired fixture を維持する。
- repository全体の `LC_ALL=C LANG=C ruby scripts/coverage-off-lint.rb` が成功する。

### docs / migration

仕様変更・migrationはない。必要なら `document/06-conventions.md` のコマンドは変更せず、そのまま locale-independent にする。
