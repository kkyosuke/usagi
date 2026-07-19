# 10. workspace-root scope（root で Agent/Terminal を作成する）

> [設計提案一覧](README.md) ｜ [ドキュメント目次](../README.md)

TUI sidebar の `⌂ root`（workspace root / `main` チェックアウト）を active にしたまま Agent と
Terminal を作成・所有・操作できるようにする設計。実装契約は
[04-ipc.md](../04-ipc.md) / [05-daemon.md](../05-daemon.md) / [03-tui.md](../03-tui.md) の各正本へ
畳み込み済みで、本書は**設計判断の根拠**を残す（実装 issue #363–#368）。

## 目次

- [背景](#背景)
- [設計判断](#設計判断)
- [scope と fence の語彙](#scope-と-fence-の語彙)
- [trusted root path](#trusted-root-path)
- [restart / reconnect](#restart--reconnect)
- [pane projection と live IO](#pane-projection-と-live-io)
- [security invariants](#security-invariants)

## 背景

v2 の leaf 型（`TerminalLaunchScope` / `TerminalRef`）は当初から `session_id: Option<SessionId>` を
持ち、`None` を workspace root として予期していた。しかし実行時経路は session を必須としていた:

- daemon terminal は `session_id == None` を「durable session fence が無い」として早期拒否し、
  generic terminal coordinator は `CompletionFence.session_id`（必須）で terminal を fence していた。
- agent dispatch は `Agent` / `LaunchScope` / `CallerRef` / `WorkerRef` / `AgentRuntimeRef` /
  dispatch inbox / `AgentLaunchIntent` がすべて `SessionId` を必須とし、root scope を表現できなかった。
- TUI は agent pane host を `SessionId` で keying し、`Target::Root` を live pane 対象外とし、
  controller が root の agent 起動を拒否していた。

## 設計判断

**root を session とは別の安全な durable scope として表現し、`session_id: Option<SessionId>` を
共有語彙に一貫して通す**（`None` = workspace root）。pseudo-session（root を偽の session record として
扱う）案は採らない。pseudo-session は session 一覧・命名一意制約・remove/worktree 操作へ漏れ、
「root は session ではない」という不変を崩し、session scope の隔離を回帰させる危険があるためである。
Option 化は leaf 型の既存方針と一致し、session 経路は常に `Some` を通すことで意味論を保つ。

## scope と fence の語彙

`Option<SessionId>` を次へ通す（`None` = root）:

| 型 | 変更 |
|---|---|
| `CompletionFence.session_id` | `Option<SessionId>`。terminal/agent の fence は `terminal.session_id == operation.session_id`（Option 同士）で成立する |
| `AgentRuntimeRef.session_id` | `Option<SessionId>`。`new` は `terminal.session_id == session_id` を検証（root runtime は root terminal のみ、session runtime は同 session terminal のみ） |
| `Agent` / `CallerRef` / `WorkerRef` / `LaunchScope` | `session_id: Option<SessionId>` |
| `AgentLaunchIntent.session` | `Option<SessionId>` |
| dispatch inbox | `None` は予約セグメント `workspace-root`（UUID と衝突しない）で分離 |

session lifecycle reducer は managed session 専用で、root operation は到達しない。fence 比較は
`fence.session_id == Some(session.session_id)` として session 経路の等価性を保つ。

## trusted root path

client は path / worktree identity / argv を一切供給しない。workspace root チェックアウトには
**永続化した root `WorktreeId`** を与える（typed ID の「不透明な incarnation・名前や path から導出しない」
方針に従い、乱数 UUIDv4 を一度だけ生成して sessions.json の envelope に保存）。daemon はこれを snapshot で
client に公開し、launch 時に要求 `worktree_id` が自分の root worktree と一致することを検証してから、
**trusted repository root** を cwd として解決する。root scope の cwd は常に daemon の
`repository_root()` であり、client の値は使わない。

## restart / reconnect

root `WorktreeId` は envelope に永続化されるため restart をまたいで安定する。既存 `sessions.json`
（root worktree 欄が無い旧版）は daemon open 時に一度だけ backfill する。restart 後の root terminal/agent は
既存の generation ownership で fence され、trusted root cwd で復元される（session と同一経路）。

## pane projection と live IO

TUI の agent pane host を `HashMap<Target, PaneRuntime>` に変更し、`Target::Root` と `Target::Session`
を同一に扱う（`PaneRegistry` は元から `Target` keyed）。`Target::Root` active でも
`OpenTerminal` / `LaunchAgent` が pane を要求し、live 判定・入力・resize・detach/reconnect が session と
同様に動く。controller は active target から `session: Option<SessionId>` を導出し、root の agent 起動を
拒否しない。

## security invariants

- root scope は client 供給の path / argv / identity を受け付けない。cwd は daemon の trusted root のみ。要求された root `WorktreeId` は daemon が自分の永続値と照合し、一致しなければ spawn 前に拒否する。
- session scope の隔離・fence を回帰させない（session 経路は常に `Some`、比較は Option 同士）。
- root の durable operation は session lifecycle reducer に混ざらない。inbox は予約セグメントで分離する。
