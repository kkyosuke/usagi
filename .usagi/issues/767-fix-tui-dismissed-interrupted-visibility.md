---
number: 767
title: 削除した interrupted を Garden とサイドバーの一覧・件数から除外する
status: todo
priority: medium
labels: [bug, tui]
dependson: []
related: []
created_at: 2026-09-17T00:00:00+09:00
updated_at: 2026-09-17T00:00:00+09:00
---

## 問題

interrupted タブを削除しても Garden の右側一覧に履歴が残る。左サイドバーも同じ runtime 集合を
使っており、削除状態を反映していない。実環境の保存データは未確認だが、コード上で表示経路の不整合を確認した。

## 原因

- `dismiss_interrupted_history` は表示 intent に `DismissInterrupted` を保存してタブを除去する。
- interrupted タブの投影は dismissed な continuation を除外する。
- workspace の表示投影は runtime phase 一覧から session ごとの Agent 群を作り、Garden とサイドバーへ渡すが、削除 intent を参照しない。

## 対応方針

会話の stable identity と保存済み表示 intent に基づく表示判定を共通化し、Garden と左サイドバーの
一覧・描画・件数へ適用する。削除・再表示時の投影と cache 更新も確認する。
表示目的で daemon の履歴を破壊せず、現在の live runtime や別の会話を誤って隠さない。

## 受け入れ条件

- running と interrupted が同居する session で interrupted 削除後は running だけを表示・集計する。
- 最後の interrupted 削除後に空状態と件数が一致する。
- 削除直後、再接続、workspace 切替後も非表示状態を維持する。
- 未削除履歴・別 session・明示的な再表示を回帰テストで確認する。
- 関連仕様を更新し、selected tests と CI を通す。

## 提出順序

ユーザー指示により、まず issue ファイルだけをコミットして Draft PR を作成し、
その後同じ PR に実装修正を追加する。
