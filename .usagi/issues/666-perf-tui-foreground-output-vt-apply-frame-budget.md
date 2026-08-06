---
number: 666
title: perf(tui): foreground output の VT apply を frame budget 内へ分割する
status: todo
priority: high
labels: [review, v2, tui, terminal, uiux, performance, rendering, scheduler]
dependson: []
related: [527, 637, 659, 660]
parent: 664
created_at: 2026-08-06T20:39:25.690834+00:00
updated_at: 2026-08-06T22:16:15.188518+00:00
---

## Finding（P1 responsiveness / display reflection）

foreground `Resume` IPCはbackground pumpへ移動済みだが、completionのclient-side適用は依然としてunboundedである。

- `TerminalPollPump::take` はcursor以降のpending chunkを全件popして1つの `Vec<TerminalChunk>` で返す。
- pendingはterminalごとに最大4 MiBまで保持される。
- `TerminalSession::poll_at` → `apply_at` は返された全chunk / byteを同じHome tickで `VtScreen::advance` へ渡してからrender/inputへ進む。

latest mainのrelease probeでは、既にO(1) evictionへ直した後でも、24×80で4 MiBのoverwrite stream適用が約25ms、scrollback cap上の改行主体4 MiBが約889msだった。絶対値はmachine依存だが、「1 tickが最大4 MiBを一括parseする」構造とframe budget超過は不変である。burst中は出力反映を急ぐほどinput/scroll/quitを長く止める。

## 修正方針

- pump drain APIをbyte/chunk budget付きにする。1 frameで適用する上限は固定byte数とmonotonic time slice（例2〜4ms）の小さい方とし、未処理suffixは同じterminal bufferに残す。
- chunk途中で分割する場合も `start_offset` / `end_offset` とbyte境界を正しく更新し、UTF-8 / CSI / OSCの途中はparser stateへ安全に跨がせる。byteをdrop/reorderしない。
- TUI threadはbudget slice適用後にprojectionを1回だけinvalidateし、残量があれば次tickを即wakeする。1 chunkごとのframe material rebuildは行わない。
- output処理よりterminal input、scroll、modal、quitの受付を飢餓させない。連続出力中も最低1回/16ms程度でinput loopへ戻る。
- pending byte、oldest age、budget yield、overflow resyncをmetrics化する。overflowは従来どおりatomic resyncへ収束し、巨大pendingを無制限に保持しない。
- exitはfinal bytesをbudgetedに適用した後で投影する。exitを先に閉じて最終出力を失わない。
- budgetはchunk数だけでなく実際に消費したbyte/timeを基準にし、巨大1 chunk、空chunk、overlap/gap、offset算術境界でも進捗0のbusy loopを作らない。time sliceが最初のbyte前に尽きても、byte hard cap内で有限の最小進捗を許すか次tickへ明示yieldする。

## 受入条件

- 64 KiB / 1 MiB / 4 MiB burstを注入しても、1回のHome loop output applyが設定したtime/byte budgetを超えない。
- burstを完全適用した最終screen/cursor/style/scrollbackが、一括 `VtScreen::advance` のreferenceと一致する。
- burst中のkey、scroll、modal、quitが規定frame数以内に処理される。
- cursor gap、pump overflow、resync、focus switch、connection epoch changeのlate bytesを誤適用しない。
- pending bufferとper-frame workはhard boundを持ち、continuous producerでもconsumerが公平に進む。
- error/exitは先行するcontiguous pending suffixをすべて適用した後に一度だけ表面化し、新しいoutputが継続する状況でも無期限に後回しにしない。
- inactive/focus切替したterminalの未適用suffixを別paneへ適用せず、再focus時はcursor・registration generationに沿って継続またはatomic resyncする。

## 必須テスト・計測

- fake pump + fake clockでbudget boundary、chunk split、multi-byte UTF-8 / CSI / OSC split、final exit orderingを検証する。
- zero budget、1-byte budget、巨大single chunk、empty chunk、cursor overlap/gap、offset上限近傍、continuous producer下のerror/exit starvationを固定する。
- deterministic frame driverで4 MiB pending中にkey/quitを挟み、最終screen parityと最大apply時間を記録する。
- release benchmarkを100/1,000/10,000 historyとoverwrite/newline burstで残す。

## 根拠箇所

- `src/runtime/terminal_pump.rs`: `MAX_PENDING_BYTES`, `PumpState::take`
- `crates/tui/src/usecase/application/terminal_session.rs`: `poll_at`, `apply_at`
- `crates/tui/src/presentation/mod.rs`: Home loopのterminal poll/material invalidation
- `crates/core/src/usecase/vt_screen.rs`: incremental parser / scrollback
