---
number: 457
title: fix(daemon): PTY child の継承環境を明示 allowlist に限定する
status: todo
priority: high
labels: [review, v2, daemon, pty, security]
dependson: []
related: [250, 251, 252, 253, 254, 255, 271, 306]
parent: 453
created_at: 2026-07-20T12:06:18.627769+00:00
updated_at: 2026-07-20T12:06:18.627769+00:00
---

## 問題・影響

root/v2 の `crates/daemon/src/infrastructure/pty.rs` にある `PtyTerminal::spawn_pair` は `portable_pty::CommandBuilder` に選択済み環境を追加するが、親環境を clear しない。generic terminal と Agent PTY child が daemon の全環境を継承し、API token などの secret を shell/Agent から読める。

## 成立条件 / 再現フロー

daemon 親に allowlist 外の sentinel secret を設定して generic/Agent の実 PTY を起動すると、child で sentinel が観測できる。`ResolvedTerminalLaunch` や `TERMINAL_ENVIRONMENT_VARIABLES` の選択処理だけでは ambient environment を消せない。

## 対象責務と非対象

単一の spawn 境界で ambient env を消し、公開 profile 変数、検証済み adapter provision 変数、daemon が発行する ephemeral credential だけを再構築する。個別 Agent CLI の機能追加、credential の wire/snapshot 永続化、任意 env passthrough の互換機能は非対象。

## 受入条件

- [ ] `PtyTerminal::spawn_pair` が親環境を明示的に clear してから allowlist を適用する。
- [ ] generic と Agent の双方が同じ契約を通り、PATH/HOME/TERM 等の許可変数は維持される。
- [ ] secret 名・値を snapshot、IPC、error、log に含めない。
- [ ] 許可変数の供給元と衝突時の優先順を 1 箇所で定義する。

## 必須回帰テスト

親に sentinel secret と許可変数を設定する実 PTY test を generic/Agent の双方に追加し、sentinel 不在・許可変数存在を child output で検証する。adapter credential の注入、空 allowlist、重複 key も固定する。

## docs / 移行影響

`document/05-daemon.md` に PTY environment 契約を追記する。暗黙の任意 daemon env に依存する child は動かなくなるため破壊的挙動として release note に載せるが、durable/wire migration はない。
