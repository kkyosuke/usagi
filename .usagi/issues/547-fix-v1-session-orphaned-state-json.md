---
number: 547
title: fix(v1/session): orphaned 隔離からの明示的な回復経路を用意する（state.json 手編集を不要にする）
status: todo
priority: high
labels: [v1, session, durability, ux]
dependson: [546]
related: [538]
created_at: 2026-07-25T02:23:46.727374+00:00
updated_at: 2026-07-25T02:23:46.727374+00:00
---

## 問題・影響

出荷 v1 でセッションが `SessionRemovalPhase::Orphaned` に隔離されると、**usagi のどのコマンドからも抜け出せない**。回復手段は `state.json` の手編集だけである。

隔離を作る経路は 2 つあり、どちらも同じ `Orphaned` 値になるため区別できない。

| 経路 | 箇所 | tombstone の中身 |
|---|---|---|
| stray 隔離 | `reconcile.rs::reconcile_locked` | `provenance` / `worktrees` が**空**（`state.json` に record が無いディレクトリ。所有権の材料が本当に無い） |
| teardown 隔離 | `mod.rs::execute_teardown`（`OwnershipError` を降格） | `provenance` は**完備**（session record 由来。所有権の材料は揃っている） |

そして出口が閉じている。

- `mod.rs::removal_target` — `Orphaned` の session への `remove` を **`--force` でも**拒否する。
- `mod.rs::resume_pending_removals` — `Orphaned` を skip する。`usagi clean` も `session::reconcile()` も回復させない。
- `mod.rs::create` — tombstone が名前を所有しているので同名再作成も拒否する。
- `mod.rs::list_statuses` — `Orphaned` を `SessionStatus` として**表示はする**が、そこから実行できる操作が無い。

結果、隔離されたセッションは「見えているのに削除も再作成もできない行」として残り続ける。#546 は teardown 隔離が起きる主要な原因（branch 乖離）を潰すが、**すでに隔離済みの workspace は救済されない**し、将来別の理由で隔離が起きたときの出口も無いままである。

なお `session::reconcile()`（公開エントリ）は production から呼ばれていない（`usagi clean` は `resume_pending_removals` を直接呼ぶ）。隔離の解除をここに置いても production には届かないので、実際に到達する surface に置く必要がある。

## 対象責務と非対象

### 対象

1. **隔離の由来を区別できるようにする**。`Orphaned` が「record 無しの stray」と「所有権証明に失敗した記録済み session」の両方を意味している状態を解消する（phase の分割、または tombstone に隔離理由を持たせる）。回復可能性の判断はこの区別に依存する。
2. **明示的な回復操作を追加する**。オペレータが隔離を解く 2 方向を用意する。

   | 操作 | 意味 | 安全条件 |
   |---|---|---|
   | 再証明して削除を続行 | 隔離時点では証明できなかった所有権を**今の実態で再証明**し、証明できたら通常の teardown を再開する | 証明できなければ隔離のまま。破壊的効果は一切出さない（#470 の fail-closed 維持） |
   | 隔離を取り下げてセッションに戻す | 生きているセッションが誤って隔離された場合、tombstone を落として record を通常状態へ戻す | session record が存在し、worktree が記録 provenance と一致することを確認できる場合に限る |

   どちらも「usagi が自動で force 削除する」経路にはしない。`Orphaned` は**オペレータの明示的な指示があって初めて**動く、という #470 の設計は保つ。
3. **到達可能な surface に載せる**。v1 に `usagi session remove` の CLI サブコマンドは無く、removal は TUI のコマンドパレット（`session remove <name> [--force]`）と MCP tool（`presentation/mcp/session.rs`）から呼ばれる。回復操作も同じ 2 surface に載せる（`usagi clean` からの自動 sweep 対象にはしない — 自動 force 削除にならないため）。
4. **エラーメッセージから次の一手が分かるようにする**。現在の `inspect state.json and \`git worktree list\`, then clean up the confirmed paths manually` を、追加した回復操作を案内する文言へ差し替える。
5. **stray 隔離（provenance が空）の扱いを明示する**。材料が無いので自動再証明はできない。オペレータが対象パスを確認して取り下げる経路だけを許し、その際に何を確認したことになるのかをメッセージで明示する。

### 非対象

- teardown が branch 乖離で隔離してしまう不具合そのもの（#546）。本 issue は「隔離されてしまったあとの出口」を担う。
- 隔離を自動的に解いて削除を進めること。オペレータの明示指示を必須にする方針は変えない。
- store lock の短命化・中断 resume（#538、完了済み）／MCP の逐次 dispatch（#539）／rename-to-trash（#541）。

## 受入条件

- [ ] teardown 由来で隔離されたセッションが、`state.json` の手編集なしに回復操作だけで削除完遂できる。
- [ ] 生きているのに誤って隔離されたセッションが、回復操作で通常のセッションへ戻り、以後の `remove` / 表示が通常どおり動く。
- [ ] 所有権を再証明できない対象は隔離のまま残り、破壊的効果が一切出ない。
- [ ] stray 隔離（provenance 空）が自動再証明の対象にならない。
- [ ] 隔離時のエラーメッセージが、追加した回復操作を案内する。
- [ ] 回復操作は TUI コマンドパレットと MCP tool の両方から呼べる。
- [ ] `usagi clean` / `resume_pending_removals` は従来どおり `Orphaned` を skip する（自動 force 削除にしない）。

## 必須回帰テスト

- teardown 隔離 → 回復操作 → 削除完遂（worktree / session tree / branch / record / tombstone がすべて片付く）。
- teardown 隔離 → 取り下げ → 通常セッションに戻り、その後の `remove` が成功する。
- 所有権を再証明できない対象への回復操作が失敗し、session tree が消えていない。
- provenance 空の stray 隔離に対する自動再証明が拒否される。
- 隔離状態のまま `create` が同名を拒否し、回復後は許可する。
- `resume_pending_removals` が `Orphaned` を引き続き skip する。
- TUI コマンドパースと MCP tool のスキーマ・ルーティング。

## docs / 移行影響

`state.json` の隔離表現（phase の値、または追加する隔離理由フィールド）が変わるため、後方互換の読み取りを用意する。既存の `orphaned` は最も保守的な側（自動再証明の対象外）へ寄せて読む。v1 の仕様ドキュメント（`v1/document/`）は退避版のため更新しない。
