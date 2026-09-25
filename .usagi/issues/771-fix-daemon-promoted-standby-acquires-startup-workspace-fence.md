---
number: 771
title: "fix(daemon): 昇格した standby が起動 workspace を fence しない"
status: todo
priority: medium
labels: [v2, daemon, lifecycle, workspace, correctness]
dependson: [773]
related: [559, 773]
created_at: 2026-09-25T00:00:00+00:00
updated_at: 2026-09-25T00:00:00+00:00
---

## 問題

standby は workspace fence も単一インスタンス lock も持たない（[document/05-daemon.md#planned replacement](../../document/05-daemon.md#planned-replacement)）。
昇格して active になった generation は `spawn_ipc_server` の中で起動 workspace を `adopt_initial` で登録するが、
`adopt_initial` は「fence は `serve` が既に取得している」前提で `fence: None` として登録するため、
**昇格した generation はその workspace を fence しないまま serve する**。

旧 owner が生きている間は旧 owner の fence が偶然その workspace を覆っていたので、この穴は観測されにくかった。
#773 で旧 owner が handoff 後にその fence を返すようになると、穴が開く時刻が旧 process の終了時から
handoff 直後（最大 30 秒後）へ早まる。旧 process の終了でも同じ穴が開くため、#773 が作った問題ではないが、
窓は広がる。

穴が開いている間、別 runtime mode / 別 `$USAGI_HOME` の daemon がその workspace の fence を取得でき、
同じ worktree・`usagi/<name>` branch・session 名に対する 2 人目の owner になりうる。

## 方針（案）

昇格した generation が起動 workspace の fence を取得する。旧 owner がまだ保持している間は取得できないので、
- 昇格時に一度試し、取得できなければ tenant sweep が再試行する、または
- `adopt_initial` を「fence を持っているならそれを使い、持っていなければ取得する」形にし、
  昇格経路だけ後者を通す。

どちらでも、取得できないまま serve し続ける状態を観測可能にして、取得できた時点で通常の tenant と同じ
扱いにする。取得できない状態が続く場合の扱い（refuse するか、fence なしで serve し続けるか）を決めることが
この issue の本体である。

## 受入条件

- 昇格した generation が起動 workspace の fence を保持している（または保持できない理由が観測できる）ことを
  test で固定する。
- 旧 owner が fence を返した後、別 process がその workspace を取得できないことを test で固定する。
