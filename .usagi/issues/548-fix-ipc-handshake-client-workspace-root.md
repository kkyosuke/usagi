---
number: 548
title: fix(ipc): handshake で client の workspace root 不一致を拒否する
status: todo
priority: medium
labels: [v2, ipc, daemon, cli, mcp, tui, correctness]
dependson: []
related: [542]
created_at: 2026-07-25T02:26:56.991011+00:00
updated_at: 2026-07-25T02:26:56.991011+00:00
---

## 問題・根拠（コード調査で確定）

IPC handshake は client がどの workspace にいるかを検証しない。[#542](542-fix-daemon-fence-workspace-mode-home.md) で「1 machine × 1 canonical workspace root に daemon は 1 つ」は成立したが、**client が意図しない workspace の daemon へ接続する経路**は fence では閉じない。

- `ClientHello`（`crates/core/src/infrastructure/ipc/mod.rs`）は `client_id` / `connection_nonce` / `expected_daemon_generation` / `supported_protocols` / `capabilities` / `required_capabilities` / `build` を持つが、**workspace の識別子を持たない**。`negotiate` が検証するのは generation・protocol・capability だけである。
- client の接続先は data directory から解決する（`current.json` → generation socket）。data directory は `$USAGI_HOME` と runtime mode で決まり、**workspace に依存しない**。
- daemon 側の workspace root は起動時に確定した 1 つだけである（`sessions.json` の `repository_root`）。

したがって workspace A で起動した daemon と同じ data directory を使う限り、workspace B の cwd から実行した client は A の session 一覧・scope・PR inventory をそのまま受け取る。利用者から見ると「別のリポジトリの session が並んでいる」状態になり、`session remove` を実行すれば A の worktree が消える。

## やること

- `ClientHello` に client の workspace 識別子を追加する（`#[serde(default)]` で後方互換にする）。
- daemon が自分の trusted repository root と突き合わせ、不一致を typed `ProtocolError` で拒否する。
- client 側は拒否を「この workspace の daemon ではない」と提示する（無言の fallback をしない）。

## 設計上の判断が必要な点

**何を「一致」とみなすかを先に決める必要がある**。これが本 issue を #542 から分離した理由である。

- client は自分の workspace root を知らない。cwd は知っているが、それが repository root とは限らない（session worktree の中、subdirectory、workspace 外のいずれもあり得る）。
- 「cwd が daemon の trusted root の配下か」を条件にすると実装は軽いが、**workspace 外から実行する正当な用途を壊す**可能性がある。現在は cwd に依らず daemon へ接続できるため、どの経路が実際に workspace 外から実行されるのかを先に洗い出す必要がある（`usagi mcp` は daemon が `USAGI_WORKSPACE_ROOT` を注入する。TUI・CLI は利用者の cwd 次第）。
- client 側で git discovery（`rev-parse --show-toplevel` 相当）を行う案は正確だが、client の起動ごとに git を実行するコストと、git repository でない workspace（mirror 経路）での扱いを決める必要がある。
- 免除が必要な経路（lifecycle 系の `daemon status` / `stop` など、workspace に紐づかない操作）を明示する。

上記を決めてから wire に載せる。決定は [document/04-ipc.md](../../document/04-ipc.md) の handshake 契約へ畳み込む。

## 受入条件

- workspace root が一致しない client の接続が typed error で拒否され、client がその理由を提示する。
- 免除すると決めた経路は従来どおり動作する。
- 旧 client（workspace 識別子を送らない）との互換方針が決まっており、テストで固定されている。
- カバレッジ 100% を維持する。[document/04-ipc.md](../../document/04-ipc.md) を更新する。
