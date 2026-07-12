---
number: 240
title: feat(tui): New の clone/register flow を backend port に結合する
status: done
priority: medium
labels: [tui, parity-b]
dependson: [230]
related: []
parent: 227
created_at: 2026-07-12T21:12:33.522307+00:00
updated_at: 2026-07-12T22:53:23.934664+00:00
---

## 目的

New の Clone/Existing form を project/git/registry port に接続し、成功時 Home、失敗時 form 保持を実装する。

## スコープ

- validation、clone/register progress、retry、成功後 Home attach。

## 対象外

- directory picker の UI 詳細、backend の git/registry 実装。

## Acceptance ID

- `B-NEW-1`。

## 依存

- #230。backend は project/git/registry の既存 port を使用する。

## 検証

- fake backend scenario で Clone/Existing の success/failure/form retention を確認する。
