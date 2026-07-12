---
number: 245
title: test(tui): parity fake reducer scenario と golden visual suite を整備する
status: done
priority: high
labels: [tui, test, parity]
dependson: [228, 231, 232, 233, 237]
related: []
parent: 227
created_at: 2026-07-12T21:12:34.116229+00:00
updated_at: 2026-07-12T23:07:04.697474+00:00
---

## 目的

A acceptance の fake reducer scenario と visual golden を reusable test suite として整備する。

## スコープ

- fake backend の event script、lifecycle/pane/phase/quit scenario、frame golden fixture。
- ANSI/CJK/wide/tiny geometry と error redaction の regression fixture。

## 対象外

- real PTY black-box、daemon socket E2E、data migration。

## Acceptance ID

- release quality: fake reducer scenario / golden visual。

## 依存

- #228/#231/#232/#233/#237。

## 検証

- test suite を CI-compatible な deterministic fixture として実行する。
