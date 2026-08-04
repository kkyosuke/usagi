---
number: 642
title: fix(tui): 失敗した session Closeup の agent/terminal 起動が理由を出さず launcher へ戻る
status: done
priority: high
labels: [tui, bug, agent, closeup]
dependson: []
related: []
created_at: 2026-08-04T00:29:48.778875+00:00
updated_at: 2026-08-04T00:31:13.302726+00:00
---

## 症状

session の Closeup で `agent`（または `terminal`）を実行すると、pending tab が一瞬現れてから daemon 側の起動失敗（例:
`that agent CLI is not installed` / readiness 不成立 / daemon 不通など）で消え、Closeup の action launcher が
何の理由も表示せずに再度開く。ユーザーからは「agent が開かない、失敗してまた Closeup が開く」としか見えず、
Enter が無反応なキーなのか実際に起動が拒否されたのか区別できない。

claude / codex / sakana.ai のどの CLI でも同様に発生し、初回起動でも再現する（provider 固有の問題ではない）。

## 原因

`PaneEvent::Failed`（`crates/tui/src/usecase/application/pane.rs` の `fail()`）は daemon が返した safe message を
`PaneState.error` に保存するが、この値を読んで画面に出す経路が **workspace-root の Director drawer 用の
projection（`director_drawer_projection`, `crates/tui/src/presentation/mod.rs`）にしかなく**、session-scoped
Closeup が使う `home_frame_material` / `HomeProjection` には無かった。

一方、pending tab が消えて `has_pane_tab` が `false` になると
`AppEvent::PaneTabAvailability`（`crates/tui/src/usecase/application/controller.rs`）のハンドラが
`state.overlay = Some(Overlay::Closeup)` で launcher を再度開く。これは正常な Agent 終了時と共通の経路で、
`PaneState.error` を一切参照していなかったため、失敗理由が握りつぶされたまま launcher だけが再度開いていた。

`document/03-tui.md` の「Closeup Agent の手動確認」表は「安全な error modal が表示され、日次 error log に
記録される」と記載していたが、実際には session-scoped Closeup にそのような modal も日次 error log への
記録も存在しなかった（記載が実装より先行していた）。

## 修正内容

- `AppEvent::PaneTabAvailability(bool)` を `PaneTabAvailability { available: bool, error: Option<String> }` に拡張し、
  `WorkspaceRuntime::sync_live_pane` が `active_pane().error()` を渡すようにした。
- launcher 再表示（`has_pane_tab` が `true → false` かつ `has_live_pane` が既に `false`）のとき、`error` が
  `Some` なら `state.notice` にその safe message を積むようにした（`None` の通常終了は従来どおり無言で戻る）。
- `document/03-tui.md` の該当行を実装に合わせて修正（error modal / 日次 error log への言及を削除し、
  再度開いた launcher の notice として表示されると記載）。

## テスト

- `crates/tui/src/usecase/application/controller.rs`: `failed_pane_launch_restores_the_launcher_with_a_notice` /
  `clean_pane_exit_restores_the_launcher_without_a_notice`（reducer 単体）。
- `crates/tui/src/presentation/workspace_runtime.rs`: 既存の `failed_launch_restores_the_action_launcher` に
  notice の assertion を追加（`WorkspaceRuntime::sync_live_pane` を含む本番経路の end-to-end）。
- `cargo test -p usagi-tui --quiet`（1078 passed）/ `cargo clippy --workspace --all-targets -- -D warnings`（warning 0）。
