---
number: 151
title: fix(tui): session 作成/削除アニメーションを Switch/Closeup 両モードで動かす
status: done
priority: medium
labels: [bug, tui]
dependson: []
related: []
created_at: 2026-07-08T22:16:16.370509+00:00
updated_at: 2026-07-08T22:38:28.084187+00:00
---

## 背景 / 問題

session の作成・削除中に表示されるサイドバーの skeleton アニメーション（`HomeState::pending_sessions()` に基づく `pending_session_rows` / `rail_pending_session_rows`。作成は青系のロード波、削除は `leaf_loading_chip` の緑波）が、**Switch モードでは動くのに Closeup モード（特に埋め込みターミナルにアタッチ中）では固まる**。モードに関わらず滑らかにアニメするようにしたい。

## 根本原因

skeleton のフレームは `ui::sidebar::skeleton_frame(now)` の通り**壁時計（90ms 刻み `SKELETON_TICK_MS`）から算出**され、再描画されるたびに波が進む。つまり「一定間隔で再描画され続けること」が動きの前提。

- **Switch モード**（`src/presentation/tui/home/event/mod.rs` のメイン `event_loop`）:
  作成/削除はバックグラウンドタスクとして走るため `panel_animating = install_task::handle().is_active(now) || tasks.is_active(now)` が真になり、`skip_paint` が偽・`animate` が真になって `install_task::ANIM_TICK` で速く回る。結果的に skeleton が滑らかに動く（`pending_sessions` に依存せず、たまたまタスク稼働で回っているだけ）。
- **Closeup / アタッチ中**（`src/presentation/tui/home/terminal/pane.rs` の pane ドライブループ）:
  再描画はシェル出力・ユーザー操作・`IDLE_REEVAL`(200ms) の再評価でしか起きず、`interactive` を立てる条件に `pending_sessions`（やバックグラウンドタスク稼働）が**含まれていない**。そのため rail の作成/削除 skeleton がアタッチ中は固まる。
- **Closeup / Focus モーダル**（メインループだが `state.mode() == Mode::Switch` でないため `skip_paint` は常に偽）:
  再描画自体は起きるが `animate` に `pending_sessions` が無いため、`watch_sessions` の `WATCH_SESSIONS_TICK`(500ms) 間隔となりカクつく（live session やタスク稼働が別途あれば速くなる）。

## 変更方針

「pending session（作成/削除 skeleton）が存在する間はアニメーションのために再描画を回す」を、両ループで明示的に扱う。

1. `src/presentation/tui/home/event/mod.rs`（メインループ）
   - `animate` 条件に `!state.pending_sessions().is_empty()` を追加し、pending がある間は `ANIM_TICK` で速く回す（Closeup-Focus / Switch 双方で滑らかに）。
   - `skip_paint` の「moving part」判定にも pending session を加え、pending がある間は再描画をスキップしない（`panel_animating` に暗黙依存せず明示化）。

2. `src/presentation/tui/home/terminal/pane.rs`（アタッチ中の pane ループ）
   - `state.pending_sessions()` が非空の間は、skeleton の壁時計フレーム更新に合わせて周期的に再描画するよう `redraw_deadline` を設定する（`SKELETON_TICK_MS`〜`MIN_FRAME` 程度の間隔）。フレームが前回描画時から進んでいれば `interactive` を立てる、あるいは `wait` にアニメ用デッドラインを渡して定期起床させる方針でよい。`skeleton_frame` は `ui::sidebar` に既にあるので再利用する。

いずれもサイドバー（Full / Rail）で `pending_session_rows` / `rail_pending_session_rows` が描かれるため、rail 表示のアタッチ中も動く必要がある。

## 受け入れ条件

- session 作成中・削除中の skeleton アニメが、Switch / Closeup（Focus モーダル・埋め込みターミナルにアタッチ中）いずれでも滑らかに動く。
- 作成/削除が終わると skeleton が消え、通常の再描画ペース（アイドル時は無駄に回さない）に戻る。pending が無いアイドル画面の再描画コストを増やさない。
- クリーンアーキテクチャの依存方向を維持。

## テスト・確認

- 既存のカバレッジ 100% を維持（`cargo fmt` / `cargo clippy --all-targets -- -D warnings` / `cargo test`。pre-push で `cargo llvm-cov`）。
- 可能な範囲でユニットテストを追加（例: pending session がある間はメインループの `animate` が真になる／`skip_paint` が偽になる、pane ループのアニメ用デッドラインが pending 時に設定される 等、既存の `event`・`terminal` のテスト方針に合わせる）。
- 手動確認: 作成/削除を走らせながら Switch→Closeup(Focus)→アタッチと遷移して skeleton が固まらないこと。

## 関連コード

- `src/presentation/tui/home/ui/sidebar.rs`（`skeleton_frame` / `SKELETON_TICK_MS` / `pending_session_rows` / `rail_pending_session_rows` / `leaf_loading_chip`）
- `src/presentation/tui/home/event/mod.rs`（`animate` / `skip_paint` / `ANIM_TICK` / `WATCH_SESSIONS_TICK`）
- `src/presentation/tui/home/terminal/pane.rs`（drive ループ / `wait` / `redraw_deadline` / `IDLE_REEVAL` / `MIN_FRAME`）
- `src/presentation/tui/home/state/mod.rs`（`pending_sessions()` / `PendingSession` / `PendingSessionKind`）

## ドキュメント

必要なら `document/design/` のサイドバー / アニメーション記述を実装に合わせて更新する。
