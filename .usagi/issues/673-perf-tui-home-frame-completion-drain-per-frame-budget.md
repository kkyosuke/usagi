---
number: 673
title: perf(tui): Home frame の completion drain を per-frame budget 内へ制限する
status: todo
priority: medium
labels: [review, v2, tui, daemon, uiux, performance, scheduler, responsiveness]
dependson: []
related: [527, 551, 665, 666, 671, 672]
parent: 664
created_at: 2026-08-06T22:33:28.840112+00:00
updated_at: 2026-08-06T22:33:28.840112+00:00
---

## Finding（P2 scheduler fairness / input starvation）

Home frameはdaemon RPCや一部workerをbackgroundへ移した一方、ready済みcompletion/actionをinput前に**空になるまで**drainする経路を複数持つ。

- `DaemonBackend::drain_events` はunbounded `mpsc::channel`を `try_iter().collect()` で全件回収し、frame loopは返った全 `AppEvent` を順番にreduceしてから次へ進む。
- `drain_controller_host_actions`、`drain_session_completions`、`drain_pane_completions_into_runtime`、restore completion drainも `while let Ok(..) = try_recv()` で全件処理する。
- `MetricsBackend` もportが返した全updateを同frameで適用する。

crosstermの`EventPump`自体はreadyなterminal inputを優先するが、Home loopはこれらdrainを終えた**後**でしか `term.read_key()` を呼ばない。producer bug、reconnect/restore burst、多数のworker completion、将来のdisplay observer追加でqueueが継続的にreadyになると、1 frameのworkとmemoryにhard boundがなく、scroll・modal・quitを含むinputへ到達できない。#527は「per-frame completion drainをboundedにする」を対象責務に含めたが、現行実装にはbackground exitの8件bound以外、共通のdrain budgetが残っていない。

## 修正方針

- frame loopのready workへ共通 `FrameWorkBudget`（laneごとの件数上限 + monotonic time slice）を置き、1 laneを空にしてから次へ進むのではなくround-robin/優先度付きで有限sliceだけ処理する。各lane内FIFOとtyped causal orderは維持する。
- slice後は必ずprojection/draw/inputへ戻る。残件があれば次tickを即wakeするが、continuous producer下でもbusy loopにせず最低1回/16ms程度でinputを観測する。
- display-only snapshot（metrics、同identityのinventory/refresh等）はpending最新1件へcoalesceできる。operation final、decision resolve、session create/remove、Agent tab intent commit等のdurable/causal completionはdrop/coalesceせずbounded admissionまたはtyped backpressureを使う。
- channel自体も件数/bytesのhard boundを持たせる。lossy laneのdrop/coalesceはcounterとsafe degradationへ出し、lossless laneの満杯をsilent successにしない。
- stale/duplicate completionのfence判定もbudget内で行い、stale floodだけでframeを占有させない。lane間で一方が常時readyでも他laneとinputを飢餓させない。
- workspace leave/quit時は新規admissionを閉じ、必要なlossless completionの所有権を明示してcleanupする。queueを全drainするまでUI終了を待たない。

## 受入条件

- 各laneへ1万件以上のready event/action/completionを投入しても、1 Home iterationの処理件数/timeが設定budgetを超えず、draw・scroll・modal・quitが規定frame数以内に進む。
- continuous producer中もbackend/host/session/pane/restore/metrics各laneが有限時間で進み、特定laneまたはinputがstarveしない。
- durable completionはFIFO/operation identity/CAS fenceを維持して一度だけ適用され、display-only coalesceは最新snapshotへ収束する。
- full queue、sender disconnect、stale/duplicate flood、handler error/panic recoveryでunbounded memory、busy spin、silent drop、success捏造を起こさない。
- idle/通常負荷では余計なframe wakeやlatencyを増やさず、既存のbackground exit 8件boundと各worker admission boundを維持する。

## 必須テスト・計測

- deterministic frame driver + fake clockでlane別burstとcontinuous producerを作り、per-frame visit counter/time budget、round-robin fairness、input/quit frame数をassertする。
- durable FIFOとdisplay snapshot latest-coalesce、queue full/backpressure、stale/duplicate、disconnectをtable-drivenに固定する。
- 10/1,000/100,000 completionでmax frame work、pending memory、drain完了までのframe数をbenchmarkする。
- 実daemon/PTY fixtureでreconnect、Agent launch/restore、session refresh、metrics更新を同時burstさせ、scroll/daemon modal/Esc/quitのwall-clock boundを確認する。

## 根拠箇所

- `crates/tui/src/usecase/application/daemon_backend.rs`: `Completions`, `DaemonBackend::drain_events`
- `crates/tui/src/presentation/mod.rs`: Home frame loopと各 `drain_*`
- `crates/tui/src/presentation/metrics.rs`: `MetricsBackend::{poll,drain_events}`
- `src/tui_input.rs`: input/backend/tick multiplexing
- `.usagi/issues/527-perf-tui-terminal-polling-ui-loop-foreground-cadence.md`: per-frame completion drain bound契約
