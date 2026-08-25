---
number: 336
title: fix(tui): 没入中に完了した session 作成/削除の skeleton が残り続ける
status: done
priority: medium
labels: [bug, tui]
dependson: []
related: []
created_at: 2026-07-10T13:29:38.545662+00:00
updated_at: 2026-07-10T13:52:56.259694+00:00
---

## 症状

session 作成中に表示されるサイドバーのローディング skeleton が、他の session を没入（Attached）で操作している間に作成が完了すると、完了後も消えずに残り続ける。削除（remove）の skeleton も同様。あわせて、完了の結果ログ行・session リスト反映・削除成功時の pool 退避も没入中は適用されない。

## 再現手順

1. session 作成を開始する（サイドバーに skeleton/ローディング表示が出る）
2. 作成完了を待たずに別の session に切り替え、Enter で没入（Attached）して操作する
3. 作成が完了しても skeleton が消えず、アニメーションしたまま残る（detach して 選択 に戻るまで残る）

## 原因

## 修正方針

- 外側ループの完了適用ブロックを共有関数（例: `event::apply_task_completions(state, tasks, evict_pool, focus_epoch) -> bool`）へ抽出する（`event/mod.rs` はカバレッジ対象なのでユニットテスト可能）。
- 没入の `drive()` ループでも毎パス（pool 借用の外で）同じ関数でドレイン・適用する。auto-focus（集中への遷移）は没入中は適用しない（`focus_epoch: None`。ユーザーは attach のために入力しており epoch が進んでいるため、外側ループでもどのみちスキップされる挙動と一致）。
- 削除成功時の pool 退避は既存の `evict_pool` と同じ処理（`pool.remove_under` + `open_panes_store::clear`）を共通化して没入側にも配線する。
- どのモード・どの session にフォーカスしていても完了時に skeleton が確実に除去されることをテストで固定する（スレッド/IO は注入で分離、カバレッジ 100% 維持）。
