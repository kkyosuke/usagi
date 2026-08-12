---
number: 669
title: fix(tui): stream 失敗した live pane を再 attach で回復させる
status: done
priority: high
labels: [v2, tui, terminal]
dependson: []
related: [527, 571]
created_at: 2026-08-12T05:17:24.597437+00:00
updated_at: 2026-08-12T05:50:22.298237+00:00
---

## 症状

managed session の Agent を Closeup で開いて操作したあと、指示モード（Director drawer）から root Agent を
起動すると、**元の session の Agent tab がフリーズする**。出力が止まり（画面が静止）、キー入力も届かず、tab を
切り替えても drawer を閉じても戻らない。TUI を再起動するまで回復しない。

実機（frozen session, 13:35–14:02）の foreground poll lane summary は
`26296 fetches (2627 with output), 1 errors, 1 fenced drops, 0 coalesced, 0 overflow resyncs, 164 wakes`
で、**stream 失敗はちょうど 1 回**、そのあと回復の試行が 1 度も無い。

## 原因

`TerminalSession` が exit 以外の失敗を「二度と回復しない状態」に落としていた。

- `fail_at` は `Unavailable` / `InputEffectUnknown` にだけ `retry_at` を予約し、
  `Stale` / `OrderingMismatch` / `ResyncRequired` は `Disconnected`、`Orphaned` は `Orphaned` にして
  `retry_at = None` を書いていた。
- `poll_at` は `Reconnecting` しか再 attach しないため、この 2 状態に落ちた pane は以後どの frame でも
  何もしない。出力は止まり、`send_input` は `NotLive` で拒否され続ける。
- pane を再 attach する経路は他に無い（`sync_foreground_terminal` は attach 済み session を保持したままにする）。
  したがって回復手段は TUI の再起動だけになる。

指示モードの foreground 受け渡しがこの分岐を日常操作に乗せる。drawer を開くと managed pane は detach され、
root Agent の launch でもう 1 つ runtime が増え、drawer を閉じると再 attach する。この attach と直後の
`Resume` が一過性の refusal を受けた瞬間に、pane は永久に死ぬ。

## 対象責務

exit 以外の失敗は attach が唯一の回復手段である（subscription を取り直し、daemon の atomic checkpoint から
screen を組み直し、daemon の `next_input_seq` を採用する）。したがって失敗の種類で運命を分けず、既存の
100ms〜2s の指数 backoff で再 attach する。明示的な detach（background へ回した pane）は従来どおり
自分から attach を奪い返さない。両者の区別は状態名ではなく**予約された再試行の有無**で行う。

回復するようになった結果、入力順序の所有者も移る。fence は queue を空にする前に外れるため、drain が失敗で
中断されると「fence 無し・queue 有り」が残る。以前はその pane が永久に死んでいたので後続の keystroke は
存在しなかったが、回復する今は新しい keystroke が待機中の入力を追い越しうる。

## 受入条件

- [x] `Stale` / `OrderingMismatch` / `Orphaned` の `Resume` 失敗、および `Stale` / `OrderingMismatch` /
      `ResyncRequired` / `Orphaned` の attach 失敗のいずれからも、backoff 満了後の poll で再 attach し、
      成功すれば live に戻って入力を受け付ける。
- [x] backoff 満了前の poll は attach を発行しない。
- [x] 明示的な `detach` は `retry_at` を持たず、いくら poll しても自分から再 attach しない。
- [x] drain が中断されて残った queue は、後続の keystroke に追い越されず、live へ戻った frame で古い順に届く。
- [x] 再 attach で live へ戻った pane は、失敗の種類にかかわらず `Reconnected` feedback を発行する。
- [x] 指示モードで root Agent を起動して drawer を閉じたあと、managed session の Agent tab が live のまま
      入力できることを実 daemon・実 PTY の E2E で固定する（detach 中に retained journal を追い越させ、
      再 attach が resync 経路を通ることも含む）。refusal 自体の注入経路は実 daemon に無いため、
      拒否からの回復は unit test が port 越しに固定する。
- [x] `document/03-tui.md` に stream 失敗の回復契約を記載する。
