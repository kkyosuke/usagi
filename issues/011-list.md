---
number: 011
feature: list
title: usagi list / status（全セッションの俯瞰）
status: todo
priority: medium
category: cli
dependson: [003]
ref: usagi.ai issue/list.md
---

# `usagi list`（または `status`）

## 概要

リポジトリ内の全セッション（worktree）の状態を一覧で俯瞰するコマンドを実装します。既存の `usagi status`（state.json への同期）を拡張し、各セッションのベース・最終更新・main との差分を可視化します。

## やること

- 全セッション（worktree）を一覧表示する。
- 各セッションのベースブランチ・最終更新時刻を併記する。
- main からの差分（ahead / behind のコミット数）を表示する。
- 既存の `usagi status` コマンドとの役割整理（統合 or 別コマンド）を行う。

## 完了条件

- `usagi list` で全セッションが ahead/behind とともに一覧表示される。
- 最終更新時刻順などで並べ替えできる。
