---
number: 660
title: perf(tui): frame material の全 clone / link scan を change-driven cache にする
status: todo
priority: medium
labels: [review, v2, tui, performance, rendering, memory, ssot]
dependson: []
related: [554, 587, 637]
parent: 654
created_at: 2026-08-05T13:49:12.068615+00:00
updated_at: 2026-08-05T13:49:12.068615+00:00
---

## Finding（P2 rendering / allocation）

#554 の redraw gate は `render_home` と `Terminal::draw` だけを skip する。比較する `HomeFrameMaterial` 自体は16ms tickごとに全量再構築されるため、idle frameでも次を払う。

- `project_controller_sessions` が全 `SessionRecord` から label/detail/path/PR summary を再生成・cloneする。
- `HomeProjection::from_state` が `state.sessions × snapshot_sessions` の nested scan（O(S²)）を行い、各 `ProjectedSession` を再cloneする。
- metrics用に全session cwdを再cloneし、`MetricsBackend` が同一thread内でmpsc send→即drainする。production `git_diffs` はactive IDの `Vec::contains`（O(S²)）と `BTreeMap` cloneを毎tick行い、`with_git_diffs`でも再cloneする。
- focused terminalはscreen revision/scroll/selectionが不変でも viewport `Vec<String>` とlink scanを毎tick再生成する。

したがって「draw 0回」のtickも、session数・viewport行数・文字列量に比例するallocation/CPUを払う。session数のhard capもないため、sidebarが増えるほどframe budgetを消費する。

## 修正方針

- daemon lifecycle revision、pane registry revision、terminal screen/output revision、controls revision、metrics/git generation、animation phaseをcheapな `FrameMaterialKey` として集約し、keyが変わったcomponentだけowned projectionを再構築する。
- session joinはstable `SessionId` keyed map / ordered projectionでO(S)にし、表示名やindexをidentityにしない。
- metrics self-channelを撤去するか、portが**変化時だけ**typed updateを返す。git worker resultとmetrics sampleをclone無し/1回のmoveでcacheへ反映する。
- terminal viewport/link projectionはterminal revision + geometry + scroll + selectionでcacheし、出力/操作が無いtickでは行Stringを作らない。
- cacheは権威にしない。controller/daemon screenがSSoTで、cacheはrevision一致時だけ再利用する派生値に限定する。

## 受入条件

- 1,000 idle ticksでsession row/path、git map、terminal rowのclone/scan回数がtick数に比例しないことをinstrumented testでassertする。
- S sessionのprojection rebuildがO(S)で、nested ID scanを持たない。
- state / terminal output / metrics / git / resize / animation / overlayを1つずつ変えたときだけ該当componentとframeが更新される。
- skipped tickでもdrain/admission/inputは従来どおり進む。
- frame内容、pointer hit-test、URL underline/click、selection/copy、preview/Director parityを維持する。

## 根拠箇所

- `crates/tui/src/presentation/mod.rs`: `project_controller_sessions`, `controller_terminal_view`, frame loop, `home_frame_material`
- `crates/tui/src/presentation/views/workspace.rs`: `HomeProjection::from_state`, `with_git_diffs`
- `crates/tui/src/presentation/metrics.rs`: same-thread channel
- `src/runtime/tui.rs`: `DaemonMetricsPort::git_diffs`
