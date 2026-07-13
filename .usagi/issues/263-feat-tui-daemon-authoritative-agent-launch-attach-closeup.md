---
number: 263
title: feat(tui): daemon-authoritative agent launch/attach を Closeup に接続する
status: in-progress
priority: high
labels: [tui, agent, daemon, pty]
dependson: [220, 232, 235, 250, 251, 252, 253, 254, 255, 256]
related: [257, 246]
parent: 227
created_at: 2026-07-13T00:15:13.466360+00:00
updated_at: 2026-07-13T00:24:45.708796+00:00
---

## 背景

v2 の Closeup registry は `agent [name]` を解釈し、controller は `OpenAgent` effect を返す。しかし effect runner は daemon client / pane runtime に接続されておらず、実行中の既存 session から Agent を起動・attach する経路がない。daemon の Codex / Claude adapter、PTY runtime、IPC client は既に存在するため、TUI が direct spawn や local fallback を持たず daemon 正本だけを使う統合 slice が必要である。

## 目的

親 usagi v2 TUI の既存 session Closeup から Agent を起動し、accepted/final の結果で同じ pane を attach する。TUI は daemon-authoritative な launch / attach API だけを通り、adapter 固有の argv・secret・raw error を扱わない。

## スコープ

- Closeup `agent` command の product-neutral な profile 選択（未指定時は daemon default policy）を定義し、controller effect と daemon IPC request へ変換する。
- producer-issued `OperationId` を request、pending Agent pane、accepted/progress/final、reconnect/replay で保つ。
- daemon の成功 completion に含まれる fenced `TerminalRef` を pane reducer / runtime へ渡し、選択中なら attach、既存 live tab は再利用する。
- daemon safe feedback を TUI projection / renderer に表示し、transport failure・unknown completion・stale generation では local spawn・再試行・attach を推測しない。
- Closeup command/effect/renderer/effect runner の一貫した UX を実装し、terminal launch (#255) と session create (#257) の state/effect を複製しない。

## 対象外

- session create と profile/model 入力 (#257)。
- generic shell terminal launch (#255)。
- Codex / Claude の argv、model allowlist、hook、MCP、secret、PTY spawn/reclaim の実装 (#250--#254)。
- TUI からの direct process spawn、PTY ownership、daemon 不通時の local fallback。

## 受け入れ条件

- session を active にした Closeup で `agent` を実行すると、TUI は stable workspace/session identity、任意の product-neutral profile、同一 `OperationId` を持つ daemon request を一度だけ送る。
- root target や不正な引数は request を送らず safe inline feedback を表示する。
- accepted から final まで Agent pending tab が表示され、成功した fenced `TerminalRef` だけを attach する。failure、stale/duplicate/unknown final、reconnect replay では誤った tab を開かない。
- attach / resync / input / resize は既存 `TerminalPort` と `PaneRuntime` を利用し、daemon 不通時にも TUI が process を spawn しない。
- renderer は pending / attach 済み / safe failure を区別して表示し、adapter private detail・argv・secret・raw wire error を表示しない。
- command parser、controller/effect adapter、pane reducer/runtime、fake IPC daemon integration、real PTY regression を追加・更新する。
- 実装済み仕様 document を更新し、Closeup の Agent launch が daemon-authoritative な launch/attach client であることを記載する。
