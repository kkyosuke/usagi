---
number: 657
title: fix(tui): New workspace clone/register を entry loop の外で非同期実行する
status: done
priority: high
labels: [review, v2, tui, workspace, freeze, git, usability]
dependson: []
related: [86]
parent: 654
created_at: 2026-08-05T13:40:18.509749+00:00
updated_at: 2026-08-05T22:34:25.659997+00:00
---

## Finding（P1 usability / freeze）

v2 の production screen graph は New form の `Enter` 後、`run_screen_graph_with_backend` の input/render thread で `loader.create_workspace(&request)` を同期呼び出しする。production adapter はその中で directory 作成、`git clone`、registry update、daemon bootstrap + lifecycle snapshot、settings/state load を実行する。

大きな repository、遅い/停止した remote、credential prompt、daemon bootstrap contention では TUI が最後の New frame のまま停止し、loading redraw、Esc/Ctrl-Q、resize を処理できない。過去の v1 issue #86 は同症状を非同期化したが、v2 screen graph に parity が移植されていない。

## 修正方針

- New operation を typed effect + completion にして worker へ移す。screen loop は draft、operation token、loading state を所有し、通常の tick/input/render を継続する。
- clone/registration/open completion は token と request identity で correlate し、late/duplicate completion が新しい draft/workspace を開かないよう fence する。
- 同時 create は 1 件（または明示 hard bound）にし、二重 Enter を coalesce/refuse する。
- failure/cancel は draft を保持し、safe notice を出す。途中で作った destination の cleanup policy を明示し、既存 directory を消さない。

## 受入条件

- hung fake clone 中も loading animation、resize、Esc/quit が応答する。
- success は既存の `open` と同じ workspace snapshot/composition へ 1 回だけ遷移する。
- failure は URL/location/branch/name/mode draft を保持し retry できる。
- double submit / late completion / leave-and-reenter / cancellation の順序を unit test で固定する。
- Welcome/Open/Config と direct workspace entry を退行させない。

## 根拠箇所

- `crates/tui/src/presentation/mod.rs`: `run_screen_graph_with_backend` の `NewStep::Create`
- `src/runtime/tui.rs`: `FsWorkspaceLoader::create_workspace`
- historical `.usagi/issues/086-feat-tui-git-clone-new.md`
