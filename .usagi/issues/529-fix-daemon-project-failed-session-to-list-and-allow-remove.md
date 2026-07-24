---
number: 529
title: fix(daemon): failed session を一覧へ投影して remove 可能にする
status: todo
priority: high
labels: [v2, daemon, tui, cli, mcp, lifecycle, ux, presentation]
dependson: []
related: []
parent: null
created_at: 2026-07-24T00:00:00.000000+00:00
updated_at: 2026-07-24T00:00:00.000000+00:00
---

## 問題・根拠

create に失敗した session（`SessionLifecycle::Failed`）は durable state に残るが、client の session 一覧に一切現れない。失敗レコードは session 名を所有し続けて同名の再作成を `workspace already exists` で拒否するのに、利用者はそのレコードの存在にも削除手段にも気付けない。

- daemon の `snapshot()`（`crates/daemon/src/usecase/session_runtime.rs`）は client へ返す `sessions` を `lifecycle == Available` だけにフィルタする。`ManagedSession` は `lifecycle` / `failure` を含む全フィールドを serialize しているため、データは既に存在し、投影が Failed 行を捨てているだけである。
- create 時の重複チェックは「retained な（= failed を含む）session record が名前を所有していれば `SessionWorkspaceExists` を返す」設計になっている。したがって Failed レコードは一覧に出ないまま名前をブロックし続ける。
- daemon 側の remove は Failed を既に完全サポートする。remove は全レコードから名前で対象を引き当て、reducer は `BeginRemove` を `Available | Failed` で受理し、`remove_session_tree` は worktree 未作成（ディレクトリ無し）でも `NotFound → Ok` で正常終了する。欠けているのは「client がその行を見て remove を呼べる導線」だけである。
- domain は `SessionLifecycle::Failed` に `can_use=false` / `can_remove=true` / `can_recover=true` の capability を与えている。presentation が Failed を投影しないことで、この remove capability が client から到達不能になっている。

つまり「Failed を durable に残す」設計は正しく（crash recovery・冪等 replay・名前所有のため）、**presentation が Failed を client へ投影しないことが設計意図と食い違っている**のが不具合の本体である。

### client 側の実態（調査で判明したトポロジ）

一覧を出す client 経路は 3 つあり、それぞれ扱いが異なる。

| client 経路 | 現在の実装 | filter 撤去だけで直るか |
|---|---|---|
| MCP `session_list` | daemon の snapshot JSON をそのまま転送する（per-row 整形なし） | ○ Failed が即可視化される |
| CLI `session` サブコマンド | **`list` サブコマンドが存在しない**（Create/Remove/Resume/RecoverLegacy/Setup/Prompt のみ） | 一覧自体が無いので別途新設が要る |
| TUI sidebar | daemon snapshot ではなく **`SessionRecord`**（`crates/core/src/domain/session/`）に射影して描画。`SessionRecord` は `lifecycle` / capabilities を持たない | × lifecycle を運ぶ射影拡張が要る |

加えて `LifecycleCapabilities`（`session_lifecycle.rs`）は serde を持たず wire に載らず、domain 外に呼び出し元が無い。client がアクションを capability で gate するには、`lifecycle` から client 側で capability を導出するか、capability を wire に載せるかの設計判断が要る。

## 対象責務

1. **daemon: 一覧用 `snapshot()` を非 Available も投影する（中核修正）**。`sessions` から `Available` 限定フィルタを外し、durable な session record を（`lifecycle` / `failure` 付きで）client へ projection する。create / remove / replay / recover_legacy が返す list 表現もすべて同じ `snapshot()` を経由するため、一貫して可視化される。これで MCP `session_list` は追加変更なしで Failed を返す。
2. **`resolve_scope` は `Available` 限定を維持する**。attach / path 解決は使用可能な session だけを対象にするべきで、変更するのは一覧用 projection だけである。使えない session に attach させる退行を作らない。
3. **TUI: sidebar が lifecycle と失敗理由を運んで描画する**。daemon snapshot → sidebar 射影（`SessionRecord` ベースの projection）に `lifecycle`（Failed の `failure.summary` を含む）を通し、Failed 行を状態付きで表示する。射影に lifecycle を載せる最小拡張で行い、未配線の別 lifecycle 経路（`SessionRow`）の全面採用には踏み込まない。
4. **client: アクションを capability で gate する**。Failed 行は `can_use=false` なので attach / 使用を提示せず、`can_remove=true` なので remove を提示する。capability は `lifecycle` から client 側で導出する（wire surface を増やさない）方針を第一候補とし、既存の remove 経路（daemon の Remove operation）をそのまま Failed 行へ配線する。
5. **recover は本 issue の非対象**。Failed の `can_recover` を満たす v2 recover 操作は現状存在しない（`RecoverLegacy` はレガシー state 採用専用で per-session recover ではない）。可視化 + remove で「気付けない・消せない」を解消するところまでを本 issue の範囲とする。

## 非対象

- Failed session を再試行する v2 recover 操作の新設（daemon operation 追加が必要。別 issue）。
- CLI の一覧サブコマンド（`session list`）新設。現状 CLI に一覧は無く、可視化の主対象は TUI sidebar と MCP `session_list` である。CLI 一覧が要るなら別 issue とする。
- 未配線の TUI lifecycle 経路（`SessionLifecycleClient` / `SessionRow`）の production 配線・全面移行。
- `resolve_scope` / attach 経路の対象拡大（Available 限定を維持する）。
- Creating / Initializing / Deleting といった過渡状態の作り込み（projection に含めてよいが、本 issue の主目的は Failed の可視化 + remove）。

## 受入条件

- [ ] create に失敗した session が `snapshot()["sessions"]` に `lifecycle = "failed"` と `failure.summary` 付きで現れる。
- [ ] MCP `session_list` の応答に Failed session が `lifecycle` / `failure` 付きで含まれる。
- [ ] `resolve_scope` は Failed session を解決せず、`ScopeUnavailable` を返す（attach 対象は Available のみ）。
- [ ] TUI sidebar が Failed session を状態（failed）と失敗理由付きで表示し、`can_use=false` のため attach を提示せず、`can_remove=true` のため remove を提示する。
- [ ] 一覧に現れた Failed session を client の remove 操作で削除でき、削除後は名前が解放されて同名 create が成功する。
- [ ] 既存の「create 失敗後は一覧が空」を前提にした daemon テストが、「Failed 行が可視化される」新挙動へ更新されている。

## 必須テスト

- daemon: create が branch/workspace 衝突で失敗した後、`snapshot()["sessions"]` に Failed 行が現れ、`lifecycle` と `failure.summary` を持つことを検証する（既存の「空である」assertion を反転。`reports_a_reusable_session_name_when_its_branch_already_exists` / `..._workspace_already_exists`）。
- daemon: 中断された reservation が restart 時に Failed へ reconcile された後、その行が projection に含まれることを検証する（現行 `len==1` 前提の更新。`resolver_requires_complete_available_scope_and_restart_reconciles_interrupted_work`）。
- daemon: `resolve_scope` が Failed session に対して `ScopeUnavailable` を返すことを検証する。
- daemon/core: Failed 行を対象にした Remove が成功し、record が消えて同名 create が通ることを検証する（worktree 未作成でも no-op success）。
- TUI: Failed 行を含む sidebar 描画と、capability に応じたアクション提示（attach 不可・remove 可）を reducer/render test で固定する。lifecycle を運ぶ射影拡張の分岐を網羅する。

## docs / gate

- `document/` の該当仕様（session 一覧・lifecycle・daemon projection）へ、一覧は Available だけでなく Failed も lifecycle 付きで投影し、Failed 行は attach 不可・remove 可であることを現在形で追記する。SSoT を崩さないよう、lifecycle capability の定義元へリンクする。
- Rust（daemon / core / tui）とテスト・coverage に影響するため、fmt / check / clippy / selected・full tests / coverage 100% / Markdown link check を必須とする。
