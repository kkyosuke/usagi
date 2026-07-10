---
number: 86
title: feat(tui): git clone を非同期化しローディング表示、失敗時は New フォームを保持する
status: in-progress
priority: high
labels: [tui, review]
dependson: []
related: []
created_at: 2026-07-03T23:20:08.870333+00:00
updated_at: 2026-07-10T20:30:31.232+00:00
---

UI/UX レビュー（2026-07 branch `usagi/ui`）由来。

## 背景 / 問題
New フォームで Enter → `usecase::project::create`（git clone）がオーケストレータのスレッドで**同期実行**される（`src/presentation/tui/app/event.rs` の `create_project` 呼び出し、`app/mod.rs`）。

- 大きいリポジトリ・遅いネットワークでは New 画面の最終フレームのまま**数十秒フリーズ**して見える。Ctrl+C も効かず、キーはバッファされるだけ。
- LLM 導入には `install_task` の全画面ローディングうさぎがあるのに、最も頻度の高い長時間処理である clone には進行表示が一切ない。
- 作成失敗（不正 URL・ネットワーク・名前衝突）で welcome へ戻され、入力した **URL・Location・Branch が全部消える**（`run_new` が毎回 `FormState::new()`）。長い URL を再入力させられる。

## 対応
1. clone を `install_task` と同じ仕組みでバックグラウンド化し、進行オーバーレイ（ローディングうさぎ）を出す。最低でも「Cloning &lt;repo&gt;…」の 1 フレームを描いてから開始する。
2. 失敗時は **New 画面へ入力値を保持したまま戻す**（`event_loop` に初期 `FormState` を渡せるようにする）。通知行にエラーを出す。
3. ドキュメント（`document/design/03-new.md`）の遷移記述を実態に合わせる（本 PR で success→Home は反映済み、失敗時のフォーム保持を追記）。

## 受け入れ条件
- clone 中に進行表示が出て、UI がフリーズして見えない。
- 作成失敗後に New フォームへ戻り、入力値が残っている。
- テストカバレッジ 100% を維持。
