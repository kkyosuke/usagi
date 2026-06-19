---
number: 37
title: Session の git submodule 対応
status: done
priority: high
labels: []
dependson: []
related: []
created_at: 2026-06-18T12:50:09.639997+00:00
updated_at: 2026-06-18T23:11:27.451182+00:00
---

# Session の git submodule 対応

## 概要

submodule を含むリポジトリで Session（worktree）を作成した際に、submodule が初期化・更新されず作業できない。worktree 作成時に submodule を適切に扱えるようにする。

## やること

- worktree 作成後に `git submodule update --init --recursive` 相当を実行する。
- submodule の有無を判定し、ない場合は余計な処理をスキップする。
- 設定で submodule の自動初期化を ON/OFF できるか検討する。

## 完了条件

- submodule を含むリポジトリで Session を作成すると、submodule が初期化・チェックアウトされた状態で作業できる。
- submodule を持たないリポジトリでも従来どおり動作する。
