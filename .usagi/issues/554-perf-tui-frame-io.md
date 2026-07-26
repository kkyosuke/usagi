---
number: 554
title: perf(tui): frame 予算からファイル IO と全画面再構築を外す
status: done
priority: medium
labels: [review, v2, tui, performance, rendering]
dependson: []
related: [527, 551, 553, 556, 557]
created_at: 2026-07-25T22:58:15.805403+00:00
updated_at: 2026-07-26T07:24:33.618723+00:00
---

## 問題・根拠（コード調査で確定）

### 1. 毎 frame のディレクトリ走査

`crates/tui/src/presentation/mod.rs` の frame loop は毎 frame `sync_runtime_sessions(&mut runtime, &ui)` を呼ぶ。この中で

```
names.extend(session_worktree_names(ui.workspace.path()));
```

が実行され、`session_worktree_names` は `<workspace>/.usagi/sessions` に対して `std::fs::read_dir` を行い、各 entry の `file_type()` を stat して `Vec<String>` を作る。呼び出し側はそれを `BTreeSet<String>` に畳んで `runtime.state().session_names()` と比較する。

用途は **inline new-session フォームの名前衝突ヒント 1 つだけ**である（関数の doc が「a read-only, best-effort preflight fact for the inline form」と明記している）。フォームが閉じている間は必要ない。tick は 16ms（`src/runtime/tui.rs` の `EventPump::new(..., Duration::from_millis(16), ..)`）なので、**idle でも毎秒約 62 回のディレクトリ走査 + entry 数ぶんの stat** が走る。session が増えるほど比例して重くなる。

### 2. 毎 tick の全画面再構築

Home は毎 frame `render_controller_frame(..)` で画面全体を組み直し、entry 画面（Welcome / Open / New / Config）も `run_screen_graph_with_backend` のループ先頭で毎 tick `welcome::render` / `render_open` / `new::render` / `config::render` を呼んで `Vec<String>` を作り直す。

端末への書き込み自体は最小である（`crates/tui/src/presentation/frame.rs` の `FrameRenderer` が `previous` frame と diff し、変わった行だけ span を出す）。しかし **frame 構築と diff の CPU は毎 tick 発生する**。mascot の瞬き以外にアニメーションが無い frame でも同じコストを払う。

## 既存 issue との境界

- [#527](527-perf-tui-terminal-polling-ui-loop-foreground-cadence.md)（done）は foreground terminal の `Resume` を UI loop から `src/runtime/terminal_pump.rs` の背景 pump へ分離した。本 issue は **分離後も frame loop に残っている非 daemon なコスト**（ファイル IO と frame 構築）を扱い、pump の内部 cadence には触らない。
  - **root のレビューが挙げた「pump が無出力 terminal に 8ms 間隔で `Resume` を投げ続ける」は既に解決済みのため本 issue に含めない**。`run_round` の戻り値 `worked` は確かに `!fences.is_empty()`（登録の有無）だが、cadence を決めているのは `worked` ではなく `PumpState::idle_rounds` である。`apply_fetch` は **出力があった場合だけ**（`produced`）`idle_rounds = 0` にリセットし、`next_interval` は `ACTIVE_INTERVAL = 8ms` から倍々で `IDLE_MAX_INTERVAL = 64ms` まで backoff する（無登録時は `UNREGISTERED_INTERVAL = 250ms`）。つまり attach 中で無出力の terminal は 125Hz ではなく最大 15.6Hz まで自動的に落ちる。keystroke / resize は `wake()` で即座に interactive cadence へ戻る。
- [#551](551-fix-tui-home-frame-loop-daemon-rpc.md) は同じ frame loop から **daemon への同期 RPC** を外す。本 issue は daemon に触らないコスト（ローカル IO と描画）を扱う。両者は同じ関数を編集するため、実装順序の調整が必要（`related`）。

## やること

- `session_worktree_names` の走査を frame 予算から外す。
  - inline create フォームが開いている間だけ、または開いた瞬間に 1 回だけ走査する。
  - フォームが長時間開かれる場合に備え、走査に cadence 上限（例: 500ms〜1s）を設ける。
- frame 構築を dirty 判定で skip できるようにする。
  - 「入力があった」「backend / drain から event が来た」「resize した」「アニメーション frame である」のいずれでもない tick は、`render_*` と `term.draw` をまとめて skip する。
  - mascot の瞬きなど時間駆動のアニメーションを、dirty 判定の入力として明示する。
- Home / Welcome / Open / New / Config のいずれでも同じ判定が効くようにする（entry 画面も同じ形のループである）。

## 設計上の判断が必要な点

- **dirty 判定の権威をどこに置くか**。reducer（`WorkspaceRuntime`）が「state が変わったか」を答えられるなら最も安全だが、`Vec<String>` の frame は state 以外（terminal 出力、metrics、git diff、mascot の時間）にも依存する。shell 側で「この tick に material が変わったか」を集約する形にするか、`FrameRenderer` の diff 結果が空だったら次 tick を skip する後追い方式にするかを決める。後者は 1 tick 遅れる代わりに実装が局所的である。
- **アニメーションの時間駆動をどう表現するか**。mascot の瞬きは frame 番号ではなく時刻に依存するため、dirty 判定に「次にアニメーションが変わる時刻」を持たせる必要がある。ここを持たないと瞬きが止まる。
- **skip が既存の副作用を落とさないか**。現在の frame loop は draw の前後で `drain_*` / `sync_*` / `restore_retry.begin_if_due` / `drain_pane_launches` を実行している。**skip してよいのは描画だけであり、drain と admission は毎 tick 走らせる必要がある**。どこまでを skip 対象にするかの線引きを明示する。
- **inline form の衝突ヒントの鮮度**。走査を 1 回だけにすると、フォームを開いている間に他 client が作った worktree との衝突を見逃す。daemon 側が権威（create は daemon が拒否する）なのでヒントの鮮度は落としてよいはずだが、UI としてどこまで許容するかを決める。
- **skip の可観測性**。skip したことが「固まった」と誤認されないよう、E2E / metrics でどう観測するかを決める。

## 受入条件

- inline create フォームが閉じている間、`.usagi/sessions` のディレクトリ走査が発生しない。
- フォームが開いている間の走査回数が cadence 上限を超えない。
- material が変わらない tick で `render_*` と `term.draw` が呼ばれない。
- material が変わらない tick でも drain / admission / 入力処理は従来どおり動作する。
- mascot の瞬きなど時間駆動アニメーションが従来と同じ間隔で更新される。
- 入力に対する応答が遅くならない（skip が入力処理を遅延させない）。
- カバレッジ 100% を維持する。`document/03-tui.md` を更新する（本 issue を実装する側が行う）。

## 必須回帰テスト・計測

- fake filesystem port（または注入した走査 port）で、フォームを開かずに N tick 進めたときの走査回数が 0 であることを assert する。
- フォームを開いた状態で N tick 進めたときの走査回数が cadence 上限以下であることを assert する。
- fake terminal で、material 不変の tick 列に対する `draw` 呼び出し回数が tick 数より少ない（理想は 0）ことを assert する。
- material を 1 つずつ変えた tick（state / terminal 出力 / metrics / git diff / resize / mascot 時刻）ごとに、その tick で `draw` が呼ばれることを assert する。dirty 判定の入力を網羅する。
- skip される tick でも drain / admission が実行されることを assert する（restore retry の admission が skip で止まらないこと）。
- 実 PTY E2E で、skip 導入後も入力→反映の frame 数が悪化しないことを固定する。
