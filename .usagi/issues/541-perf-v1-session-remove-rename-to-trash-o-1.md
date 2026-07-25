---
number: 541
title: perf(v1/session): remove を rename-to-trash + 遅延回収にして応答を O(1) にする
status: todo
priority: medium
labels: [v1, session, performance]
dependson: [538]
related: [539]
created_at: 2026-07-24T22:38:50.347251+00:00
updated_at: 2026-07-24T22:38:50.347251+00:00
---

## 問題・影響

`session remove` の所要時間の大半は `v1/src/usecase/session/reconcile.rs::discard_session` の `fs::remove_dir_all(root)` である。セッション worktree には各自の `target/` が含まれ、coverage を回した後は `target/llvm-cov-target` を含めて数 GB になる。数分かかる削除が、caller（MCP tool 呼び出し・TUI の worker）の応答時間そのものになっている。

`538` で store lock を短命化すれば他プロセスは救われ、`539` で MCP を並行化すれば同一プロセスの他要求も救われるが、**その remove 呼び出し自体は依然として数分返らない**。エージェントの tool 呼び出しは timeout を持つため、成功した削除が timeout として観測される（実際に 120 秒で打ち切られた）。

## 成立条件 / 再現フロー

1. セッション worktree で `cargo llvm-cov` 等を回し `target/` を数 GB にする。
2. `session_remove` を呼ぶ。
3. ownership preflight と git 操作は数秒で終わるのに、`remove_dir_all` が数分かかって応答が返らない。

## 対象責務と非対象

### 対象

1. **削除を rename に置き換える**。ownership preflight（`discard_session` の canonicalize / worktree 登録 / branch 照合。`470` で fail-closed 化された判定）を今どおり先に通した後、破壊的効果を `fs::remove_dir_all(root)` から **同一ファイルシステム内の `fs::rename(root, trash)`**（例 `.usagi/trash/<name>-<removal-id>/`）へ変える。rename は O(1) で、その時点で session tree は worktree パスから消えるため、既存の順序規約（「prune / branch delete の前にディレクトリを消す」）はそのまま成立する。
2. **trash の回収**。`538` で追加する resume の同期エントリポイント（`reconcile()` / `create` / `remove`）と同じ場所で trash を掃く。回収は tombstone を持たない純粋な後始末なので、失敗しても caller を失敗させない。回収経路は削除対象が `.usagi/trash/` 配下にあることを確認してから消す（fail-closed）。
3. **rename が使えない場合の fallback**。`.usagi/trash/` が別デバイスにある / rename が `EXDEV` 等で失敗する場合は、従来の `remove_dir_all` に落とす。挙動差を明示し、テストで両経路を固定する。
4. **`.gitignore` と可視性**。`.usagi/trash/` が git に載らないこと（`usagi init` の `.gitignore` 生成、`v1/src/infrastructure/gitignore.rs`）と、reconcile が trash 配下を stray として `Orphaned` 隔離しないことを保証する（stray 判定は `.usagi/sessions/` 直下のディレクトリのみを見ているが、退行させない）。
5. **削除が O(1) になった前提で、tick からの sweep 配線を検討する**。`538` で非対象とした「TUI の periodic tick から resume / 回収を掃く」配線は、重い IO が消えた後なら安全に足せる。本 issue でその可否を判断し、足す場合は worker 経由で行う。

### 非対象

- store lock の短命化（`538`）と MCP の並行化（`539`）。
- 別ファイルシステムに trash を置く設定の追加。
- worktree ごとの `target/` 共有（`CARGO_TARGET_DIR` 集約）などビルド構成側の対策。

## 受入条件

- [ ] 数 GB の `target/` を持つセッションの `session_remove` が、ディスクの実削除を待たずに（rename + git 操作 + state commit の時間で）返る。
- [ ] 返った時点で session record と tombstone が消え、worktree パスが再利用可能（同名 `create` が成功する）。
- [ ] trash 配下の実体は後続の同期エントリポイントで回収され、放置されない。
- [ ] rename 不能な環境では従来の `remove_dir_all` 経路で正しく削除される。
- [ ] trash 配下は git に載らず、reconcile が `Orphaned` 隔離しない。
- [ ] `470` の ownership fail-closed 判定が rename 前に変わらず適用される。

## 必須回帰テスト

- rename 経路: remove 後に session tree が worktree パスから消え、trash 配下に移っており、回収後に消える。
- 回収経路: trash に残ったディレクトリが次のエントリポイントで消える。回収失敗が caller を失敗させない。
- fallback 経路: rename を失敗させたとき `remove_dir_all` で削除される。
- 同名再作成: remove 直後（trash 回収前）に同名 `create` が成功する。
- fail-closed: ownership 証明が足りない場合、rename も起きない。
- trash 配下が stray 判定・git 追跡の対象にならない。

## docs / 移行影響

`.usagi/trash/` という新しいディレクトリが増える。`usagi init` の `.gitignore` に追加が必要かを確認する。既存 state.json のスキーマ変更はない（removal id が必要なら `538` の tombstone に載せる）。
