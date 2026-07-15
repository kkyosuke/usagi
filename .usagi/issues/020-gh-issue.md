---
number: 20
title: gh Issue 連携によるセッション作成
status: done
priority: low
labels: [cli]
dependson: [3]
related: []
created_at: 2026-06-16T23:02:35.152530+00:00
updated_at: 2026-06-16T23:02:35.152530+00:00
---

# gh Issue 連携によるセッション作成

## 概要

GitHub / GitLab の Issue 番号を指定してセッションを開始する機能を実装します。Issue タイトルからブランチ名を自動生成し、ワークスペースを準備することで、Issue ベースの開発フローを効率化します。

## やること

- `session new --issue <番号>`（または `usagi start --issue <番号>`）で Issue を取得する。
- Issue タイトルからブランチ名を自動生成し、セッション（#003）を作成する。
- GitHub CLI（`gh`）等を利用して Issue 情報を取得する。

## 完了条件

- Issue 番号を指定すると、タイトル由来のブランチ名でセッションが作成される。
- `gh` 未導入時は分かりやすいエラーを表示する。
