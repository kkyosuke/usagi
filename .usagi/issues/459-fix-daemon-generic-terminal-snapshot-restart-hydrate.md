---
number: 459
title: fix(daemon): generic terminal snapshot を restart 時に hydrate する
status: todo
priority: high
labels: [review, v2, daemon, terminal, durability]
dependson: []
related: [209, 255, 264, 311, 350, 365, 386, 458]
parent: 453
created_at: 2026-07-20T12:06:19.331895+00:00
updated_at: 2026-07-20T12:07:40.568263+00:00
---

## 問題・影響

root/v2 の `src/runtime/daemon.rs` にある `FileTerminalStore` は `terminals.json` を save するだけで load せず、`new_terminal_runtime` は空の `GenericTerminalRuntime` / `GenericTerminalCoordinator` を作る。restart 後に旧 terminal が inventory から消え、最初の launch が旧 snapshot を上書きする。

## 成立条件 / 再現フロー

generic terminal を launch して `TerminalRef` と snapshot を保存し、daemon runtime を再生成する。inventory と旧 ref の制御を試し、その後別 terminal を launch すると、旧 record が読み込まれていないことを観測できる。

## 対象責務と非対象

terminal snapshot の load、scope/fence を保った保守的 reconcile、inventory と次回 save への保持を対象とする。旧 PTY の FD 復旧・プロセス継続は非対象で、Agent runtime は #458、output retention は #472。

## 受入条件

- [ ] spawn admission 前に snapshot を load し、旧 running/reserved record を `identity_unknown` / `live: false` として保持する。
- [ ] 旧 `TerminalRef` の input/resize/kill/attach は typed error で拒否し、別 terminal の effect を起こさない。
- [ ] 新規 launch/save で旧 record、workspace/session/worktree/generation fence を消さない。
- [ ] load failure、破損、未知 schema では spawn と snapshot 上書きを fail closed にする。

## 必須回帰テスト

実 `terminals.json` を使う 2 instance restart integration test で inventory、旧 ref 拒否、新規 launch 後の record 保持、破損 snapshot 時 effect 0 を検証する。

## docs / 移行影響

`document/04-ipc.md` と `document/05-daemon.md` に generic terminal の restart projection を追記する。旧 PTY 自体は resume せず、利用者に非 live と明示する migration とする。
