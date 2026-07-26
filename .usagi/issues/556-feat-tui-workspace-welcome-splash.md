---
number: 556
title: feat(tui): workspace から Welcome へ戻る動線と起動 splash のスキップ
status: done
priority: medium
labels: [review, v2, tui, ux, navigation]
dependson: []
related: [523, 542, 548, 551, 553, 554]
created_at: 2026-07-25T22:59:10.749919+00:00
updated_at: 2026-07-26T07:32:58.442150+00:00
---

## 問題・根拠（コード調査で確定）

### 1. workspace に入ると Welcome へ戻れない

`crates/tui/src/presentation/mod.rs` の

```
enum WorkspaceStep {
    Quit,
}
```

は単一 variant で、`drive_workspace_controller` の返り値も `Ok(WorkspaceStep::Quit)` の 1 経路しかない（`backend.dispatch(effect) == BackendFlow::Exit` のときだけ return する）。

その結果、Welcome / Open / New から workspace に入る 3 経路はすべて `Exit::Quit` に落ちる（`run_screen_graph_with_backend` 内）。

| 経路 | 戻り |
|---|---|
| `WelcomeStep::OpenRecent` | `open_snapshot_via_controller(..)?;` の後に無条件 `return Ok(Exit::Quit)` |
| `OpenStep::Choose` | `open_snapshot_via_controller(..).map(\|_\| Exit::Quit)` |
| `NewStep::Create` | `open_snapshot_via_controller(..).map(\|_\| Exit::Quit)` |

`Screen::Open` / `Screen::New` / `Screen::Config` は `*Step::Back` を持って Welcome へ戻れるのに、**workspace だけが片道**である。Overview command 側にも workspace 切替が無い（`crates/tui/src/usecase/overview/mod.rs` の `Command` は `Config` / `Env` / `Issue` / `Session` の 4 つだけ）。

したがって別の workspace を開くにはプロセスを終了して `usagi` を再起動するしかない。**複数 workspace を束ねるオーケストレータとして動線が閉じていない**。

### 2. 再起動ごとに約 1.54 秒の無入力 splash を見る

`launch_screen_graph`（`src/runtime/tui.rs`）は `start == Start::Welcome` のとき必ず `presentation::play_startup_splash(terminal)` を実行する。

```
for frame in 0..splash::FRAMES {
    let (height, width) = term.size()?;
    term.draw(&splash::render(height, width, frame))?;
    term.wait(splash::ANIM_TICK)?;
}
```

- `splash::FRAMES` = `TITLE_DELAY(5) + TITLE_FADE.len()(4) + 1 + TITLE_HOLD(4)` = **14 frame**（`crates/tui/src/presentation/views/splash.rs`、`TITLE_FADE: [u8; 4]` は `crates/tui/src/presentation/theme.rs`）。
- `splash::ANIM_TICK` = **110ms**。合計 **1.54 秒**。
- `CrosstermTerminal::wait` は `std::thread::sleep(duration)` であり（`src/runtime/tui.rs`）、**その間 `read_key` を一度も呼ばない**。したがって splash は打鍵で中断できず、スキップ手段も無い。

1 と 2 は乗算的に効く。workspace を切り替えるたびにプロセス再起動が必要で、再起動のたびに中断できない 1.54 秒を待つ。

## 既存 issue との境界

- 本 issue は frame 予算の 3 件（[#551](551-fix-tui-home-frame-loop-daemon-rpc.md) / [#553](553-fix-ipc-tui-attach-input-lane-request-deadline-bootstrap-lock.md) / [#554](554-perf-tui-frame-io.md)）とは独立した **動線（UX）の欠落**である。ただし「workspace を抜けて Welcome に戻る」を実装すると、workspace controller が保持している daemon 接続・pump・restore worker の teardown 経路が必要になるため、それらの lane 設計を変える #551 / #553 と実装順序が干渉しうる（`related`）。
- daemon 側の workspace 権威（1 daemon = 1 workspace root）は [#542](542-fix-daemon-fence-workspace-mode-home.md) / [#548](548-fix-ipc-handshake-client-workspace-root.md) が確定させている。**同一プロセス内で別 workspace を開くと、そのプロセスは別 workspace 権威の daemon へ接続する必要がある**。この制約は本 issue の設計論点であり、両 issue が正本である（`related`）。

## やること

- `WorkspaceStep` に `Back` 相当の variant を追加し、`drive_workspace_controller` が Welcome へ復帰できるようにする。
- `run_screen_graph_with_backend` の 3 経路（Recent / Open / New）が `Back` を受けて `Screen::Welcome` に戻るようにする。`Exit::Quit` はプロセス終了専用にする。
- workspace を抜けるときに、その workspace のために確立した資源（terminal lane / poll lane / pump / restore worker / metrics lane）を確実に teardown する。
- 起動 splash をスキップ可能にする。打鍵での中断、または 2 回目以降の省略、あるいは両方。
- 終了確認（Ctrl+Q / quit modal）と「workspace を出る」の意味を UI 上で区別する。

## 設計上の判断が必要な点

- **「戻る」の入力をどう与えるか**。現在 Home の Esc / q / Ctrl-Q は終了系に割り当たっている。`document/03-tui.md` の「画面と入力」と「Home と target」の既存割当を確認し、新しい binding を足すのか、既存の quit modal に「Welcome に戻る」を選択肢として足すのかを決める。**終了と離脱を同じキーに畳むと誤操作の意味が変わる**ため、ここは先に決める必要がある。
- **daemon 権威との整合**。1 daemon = 1 workspace root（#542 / #548）なので、同一プロセスで workspace B を開くには B の daemon へ接続し直す必要がある。現在は entry 画面が `WorkspaceLoader::open` で refusal を notice にして留まる形になっている。プロセス内で切り替える場合に (a) 旧 workspace の daemon 接続を完全に閉じてから開くのか (b) 複数 daemon への接続を同時に持ちうるのか を決める。
- **live pane の扱い**。workspace を抜けるとき、その workspace で attach していた pane をどうするか。daemon 側の terminal は残る（それが detach の意味）が、`Detach` を明示的に送るのか、コネクションを閉じて daemon 側に解放させるのか（#523 の epoch 契約）を決める。
- **splash のスキップ方針**。以下のいずれか、または組み合わせ。
  - 打鍵で中断: `wait` を「入力があれば早期に戻る」形に変える必要がある。`Terminal` port の契約変更になる。
  - 2 回目以降の省略: 「2 回目」の定義（同一プロセス内で Welcome に戻ったとき / 前回起動からの経過時間 / 設定）を決める。
  - 設定で無効化: `Settings` に項目を足す。既存の settings scope（`document/03-tui.md` の「settings scope と workspace entry」）と整合させる。
- **戻り先の状態**。Welcome の Recent 一覧は workspace を開いた時点で `record_opened` されている。戻ったときに一覧を再読込するのか、開いた時点の順序を保つのかを決める。

## 受入条件

- Home から Welcome へ戻れ、そこから別の workspace を開ける。プロセスの再起動を必要としない。
- 戻ったあとに再度 workspace を開いても、前の workspace の terminal lane / pump / worker が残留しない。
- 終了（プロセスを終わる）と離脱（Welcome へ戻る）が UI 上で区別でき、誤操作で意図しない側に落ちない。
- splash が打鍵で中断できる、または 2 回目以降は表示されない（採用した方針に従う）。中断しても Welcome の初期状態は正しい。
- 別 workspace を開いたときに daemon の workspace fence（#542 / #548）が正しく働き、refusal は notice として提示される（無言の fallback をしない）。
- カバレッジ 100% を維持する。`document/03-tui.md` の「画面と入力」「Home と target」「feedback と終了」を更新する（本 issue を実装する側が行う）。

## 必須回帰テスト・計測

- screen graph の遷移テストで、workspace → Welcome → 別 workspace の往復が 1 プロセス内で成立することを assert する。
- 離脱時に teardown された port / worker の数を fake port で assert する（残留 0）。
- 終了 binding と離脱 binding が別経路であること、quit modal の選択肢が意図どおりに分岐することを assert する。
- fake terminal で splash 中に入力を与え、規定 frame 数以内に Welcome に到達することを assert する（打鍵中断を採用する場合）。
- 2 回目以降の省略を採用する場合、1 回目は表示され 2 回目は 0 frame であることを assert する。
- workspace fence refusal（別 workspace の daemon）を返す fake loader で、entry 画面に留まって notice が出ることを assert する。
- 実 PTY E2E で、workspace 離脱 → 再入場が hang しないことを固定する（既存の直列化 lock を使う）。
