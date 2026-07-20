---
number: 397
title: fix(tui): Config Save 押下後の loading→saved→自動復帰 UX を実装する
status: done
priority: medium
labels: [tui, ux]
dependson: []
related: []
created_at: 2026-07-20T04:29:28.756858+00:00
updated_at: 2026-07-20T06:30:44.165625+00:00
---

## 背景 / 問題

Config 画面の Save 押下後の状態遷移が期待どおりでない。

現状（`crates/tui/src/presentation/views/config.rs` / `crates/tui/src/presentation/mod.rs`）:

- `step_config` の `Key::Enter if config.can_save()` が同期の `Config::save()` を呼び、成功で `ConfigStep::Saved` を返す。
- `run_with_settings_inner` の `ConfigStep::Saved` は「saved」frame を **1 回だけ** draw して即座に `Screen::Welcome` へ切り替える。次の loop 先頭で Welcome frame を draw するため、ユーザーは「saved」表示をほぼ視認できない。
- Save 前の loading（保存中）表示が無い。
- 保存完了の確認を保持する短い表示時間（timer）が無い。

## 期待する正常系の状態遷移

1. Save を押す → **Save button 自体が loading 表示**（`saving…`）になる
2. 保存完了後に**同じ button が `saved` 表示**へ変わる
3. 短い表示時間ののち、**ユーザー操作なしで直前の画面（Welcome）へ自動的に戻る**

## 要件

- **失敗系**: 保存に失敗したら自動で戻らず Config 画面に留まり、エラー notice を表示して再試行できる（既存の「失敗時は draft を保持」を維持）。
- **連打 / 保存中の再操作の抑止**: 保存中（`Saving`）は Save の再トリガを no-op にする。保存の実行中に入力を読まない同期モデルを維持し、保存処理が二重に走らないようにする。
- **loading 表現・時間定数の統一**: 既存 UI の loading 表現（sidebar skeleton wave 等）や時間定数（`splash::ANIM_TICK` / `SIDEBAR_DOUBLE_CLICK`）と整合する形で、保存確認の表示時間定数を 1 か所（正本）に置く。
- **描画規約の順守**: pure な状態機械（`Config`）と `#[coverage(off)]` の IO loop の分離を保つ。timer 待機は `Terminal::wait(Duration)` に委譲し、presentation テストは待機なしで検証する。

## 設計方針

`crates/tui/src/presentation/views/config.rs` の pure な `Config` に保存フェーズを追加する。

```
SavePhase = Idle | Saving | Saved
```

- `begin_save() -> bool`: `field == Save && is_dirty() && phase == Idle` のときだけ `Saving` にして true（＝連打 / 二重押下ガード）。
- `commit_save(port) -> bool`: dirty な draft を永続化。成功で `saved = draft` / `phase = Saved` / notice=`"saved"`、失敗で `phase = Idle` / notice=`"Save failed: …"`。
- `reset_save()`: 自動復帰の直前に `phase = Idle` に戻し notice をクリア。
- button ラベルは phase で切替（`Idle→"Save"` / `Saving→"saving…"` / `Saved→"saved"`）。`saving…` は dirty のため enabled（success color）、`saved` は非 dirty のため dimmed（現状の見た目を維持）。
- 保存確認の表示時間定数 `SAVED_DISPLAY: Duration`（例 600ms）を `config.rs` に置く。

IO loop（`run_with_settings_inner`, coverage off）:

- `step_config` の Enter は `begin_save()` が true のとき新設 `ConfigStep::Save` を返す。
- loop の `ConfigStep::Save` は「loading frame を draw → `commit_save` → 成功なら saved frame を draw → `term.wait(SAVED_DISPLAY)` → `reset_save` → `Screen::Welcome`」。失敗なら何もせず Config に留まる（次 loop で error notice 付き Config を再描画）。

## テスト（決定的）

pure `Config`（`views/config.rs`）:

- 正常系: `begin_save` → `Saving`、`commit_save` 成功 → `Saved` / notice `saved` / 非 dirty、button ラベルが `saving…` → `saved` と render される。
- 失敗系: `commit_save` 失敗 → `Idle` / draft は dirty のまま / `Save failed:` notice / 再試行で成功。
- 二重押下: `Saving` 中の `begin_save` は false（no-op）、`commit_save` は 1 回だけ永続化する。
- `reset_save` で `Idle` に戻り notice がクリアされる。

IO loop（`run_with_settings` + FakeTerminal, 決定的）:

- timer 前後の順序: frames に `saving…` frame → `saved` frame の順で現れ、その **後に** Welcome（`Menu`）frame が現れる。`waits` に `SAVED_DISPLAY` がちょうど 1 回記録される（キー入力なしの自動復帰）。
- 失敗系: 失敗する SettingsPort で Save → `saved` frame も `SAVED_DISPLAY` wait も無く、Config 画面に留まり error notice が出る。

## ドキュメント

- `document/03-tui.md` の Config 節に、Save 押下後の loading→saved→自動復帰と、失敗時に留まって再試行できる挙動、保存の実行中は入力を読まない（二重実行しない）ことを追記する。

## 完了条件

- 上記 UX と要件を満たす実装。
- 追加テストが正常系・失敗系・timer 前後・二重押下を決定的に検証する。
- `document/03-tui.md` 更新。
- fmt / clippy / full test / coverage 100% が CI で green、Draft→Ready。
