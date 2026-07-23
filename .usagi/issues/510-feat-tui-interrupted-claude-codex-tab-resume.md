---
number: 510
title: feat(tui): interrupted Claude/Codex を tab 単位で選択 resume する
status: todo
priority: high
labels: [review, v2, tui, agent, recovery]
dependson: [504, 506, 509]
related: [388, 503, 526]
parent: 505
created_at: 2026-07-21T21:20:53.225285+00:00
updated_at: 2026-07-23T00:09:07.139578+00:00
---

## 問題・影響

現在の TUI は daemon inventory の `live` runtime だけを pane tab へ投影し、non-live / `identity_unknown` は tab を作らない。resume 操作は managed session 行から session-scoped に発火し、workspace root を明示的に拒否する。このため daemon crash / cold restart 後、利用者は元の Claude / Codex tab を見分けたり、複数の履歴から一つを選んで復帰したりできない。

## 対象責務

#509 の resumable inventory を target-scoped `PaneRegistry` へ投影し、各 interrupted runtime を stable な別 tab として表示する。#506 の saved slot は共通 `AgentContinuationRef` で対応付け、同じ位置・selection / dismissal intent を保つ。tab は provider 種別、safe status / reason、last-known timestamp 等の非 sensitive 表示だけを持ち、raw provider ID を label、snapshot、feedback に出さない。

root / managed session の両方で、選択 tab に対する明示 `Resume` action を提供する。操作フローは次のとおりとする。

1. selected interrupted tab の opaque `AgentResumeTarget` と新しい `OperationId` を daemon へ送る。
2. 同じ tab を pending state にし、他の history と live tab を変更しない。
3. accepted / replayed final の source → replacement relation、同じ `AgentContinuationRef`、新しい exact `TerminalRef` が全て一致した場合だけ同じ tab を live に置換する。
4. 成功 tab が foreground の場合だけ attach / resync する。failure は interrupted tab を残し、provider ID を含まない safe reason と retry 可否を表示する。

TUI start、workspace open、inventory refresh、daemon reconnect / restart から resume を自動発火しない。tab close は #506 の continuation-scoped dismissal と整合し、provider conversation や runtime record を削除しない。dismissed lineage は interrupted / resume-unavailable history に残っていても再表示せず、inventory absence では解除しない。#510 では利用者の明示 reopen だけが dismissal を解除し、authoritative retention / GC と表示 intent の将来連携は #526 が所有する。live runtime が同時に発見された場合は continuation + exact replacement relation で一枚へ収束し、名前や provider 種別だけで merge しない。

## 受入条件

- [ ] daemon cold restart 後、root と managed session の複数 interrupted Claude / Codex history を stable な別 tab として表示し、再 open / duplicate inventory でも二重 tab を作らない。
- [ ] 利用者が選んだ exact tab だけを pending → new live `TerminalRef` へ置換し、他の tab、provider conversation、selection を誤変更しない。
- [ ] TUI open / reconnect / inventory / planned restart は provider resume を発火せず、明示 Resume action だけが daemon request を送る。
- [ ] root resume が managed session と同じ UX / fencing で動き、同一 session の複数履歴や Claude / Codex 混在を ambiguous にしない。
- [ ] unavailable Codex capture、provider binary 不在、stale target、adapter / scope mismatch、duplicate click、transport failure を safe feedback にし、空 Agent / local process / 二重 pane を作らない。
- [ ] provider ID、argv、cwd、transcript、raw daemon error を UI / resume state / log に露出しない。

## 必須 product E2E

shipping TUI、実 daemon process / socket / PTY、Claude / Codex fixture を使う。

1. root と managed session に複数 Agent を起動し、provider resume metadata と tab intent を保存する。
2. daemon を SIGKILL または明示 force-cold した後に fresh start し、旧 PTY が live 復元されないことと、各 history が distinct interrupted tab になることを確認する。live resource を持つ通常 `daemon stop` を cold failure の代用にしない。
3. tab を一つずつ選んで resume し、fixture argv の exact provider session ID、新 Agent PID / `TerminalRef`、spawn count、retained provider conversation marker を確認する。
4. mixed provider、same-provider multiple history、double click、TUI reconnect、failure 後 retry、tab close / reopen を含める。

fresh daemon start、TUI open、inventory projection、reconnect の各段階では provider resume invocation / replacement spawn が 0 であることを、最初の明示操作より前に確認する。double click / request replay は daemon operation 1 件、final 1 件、child spawn 1 件、resulting tab / `TerminalRef` 1 枚へ収束させる。

Codex success case は #504 の production structured capture を通す。capture 無しの case は unavailable のまま明示し、`--last` や新規空会話へ downgrade しない。

## docs / migration

[TUI](../../document/03-tui.md) に interrupted tab、明示 Resume action、pending / failure / live transition、root UX、privacy 表示を追記し、[workspace pane restore proposal](../../document/proposals/11-workspace-restore-panes.md) の live-only restore と cold-restart resume の責務境界を更新する。旧 TUI-local state に resume target が無い場合は inventory から安全に再構成し、provider ID を推測しない。
