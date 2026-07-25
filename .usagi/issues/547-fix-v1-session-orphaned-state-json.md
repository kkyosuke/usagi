---
number: 547
title: fix(v1/session): orphaned 隔離からの明示的な回復経路を用意する（state.json 手編集を不要にする）
status: done
priority: high
labels: [v1, session, durability, ux]
dependson: [546]
related: [538]
created_at: 2026-07-25T02:23:46.727374+00:00
updated_at: 2026-07-25T07:11:34.309332+00:00
---

## 問題・影響

出荷 v1 でセッションが `SessionRemovalPhase::Orphaned` に隔離されると、**usagi のどのコマンドからも抜け出せなかった**。回復手段は `state.json` の手編集だけだった。

出口が閉じていた箇所:

- `mod.rs::removal_target` — `Orphaned` の session への `remove` を **`--force` でも**拒否する。
- `mod.rs::resume_pending_removals` — `Orphaned` を skip する。`usagi clean` も `session::reconcile()` も回復させない。
- `mod.rs::create` — tombstone が名前を所有しているので同名再作成も拒否する。
- `mod.rs::list_statuses` — `Orphaned` を `SessionStatus` として**表示はする**が、そこから実行できる操作が無い。

結果、隔離されたセッションは「見えているのに削除も再作成もできない行」として残り続けた。#546 は teardown 隔離が起きる主要な原因（branch 乖離）を潰したが、**すでに隔離済みの workspace は救済されない**し、将来別の理由で隔離が起きたときの出口も無かった。

なお `session::reconcile()`（公開エントリ）は production から呼ばれていない（`usagi clean` は `resume_pending_removals` を直接呼ぶ）。隔離の解除をここに置いても production には届かないので、実際に到達する surface に置く必要がある。

### 前提（#546 で完了済み）

隔離を作る経路は 2 つあり、#546 でその由来が `PendingSessionRemoval.quarantine: Option<QuarantineOrigin>` として永続化された。回復可否の判断はこの区別に依存する。

| 経路 | 箇所 | `quarantine` | tombstone の中身 |
|---|---|---|---|
| stray 隔離 | `reconcile.rs::reconcile_locked` | `Reconcile` | `provenance` / `worktrees` が**空**（`state.json` に record が無いディレクトリ。所有権の材料が本当に無い） |
| teardown 隔離 | `mod.rs::execute_teardown`（`OwnershipError` を降格） | `Teardown` | `provenance` は**完備**（session record 由来。所有権の材料は揃っている） |

`SessionRecord` / `PendingSessionRemoval` の記録 branch（`branch: Option<String>`）も #546 で入った。所有権の再証明はこの記録 branch を使い、セッション名から導出し直さない。

## 実装した内容

### 1. 明示的な回復操作（`usecase::session::recover_quarantine`）

`QuarantineRecovery` の 2 値をオペレータが明示指定する。usagi が自動で選ぶことはない。

| 操作 | 意味 | 安全条件 |
|---|---|---|
| `Resume`（再証明して削除を続行） | 隔離を `git_teardown` へ戻し、通常の teardown を走らせる。`discard_session` が**今の実態に対して所有権を再証明**してから初めて効果を出す | 記録 provenance が非空であること。証明できなければ `execute_teardown` が隔離を**元どおり**掛け直し、破壊的効果は一切出ない |
| `Release`（隔離を取り下げる） | tombstone を落とす（所有権の材料が皆無だった場合は record も）。ファイルには一切触らない | **無傷のセッション**（記録 worktree がすべて実在し記録どおり登録されている＝`reconcile::prove_live_session`）、または **材料の無い tombstone**（記録ディレクトリが既に存在しない） |

- `Resume` は `Reconcile` 由来を明示的に拒否する（再証明する材料が原理的に無い）。`provenance` が空、または record が無い tombstone も拒否する。これで legacy（`quarantine` が `None`）でも「証明できる材料があるか」という実体で判定され、stray が自動再証明に載ることはない。
- `Release` の「材料の無い tombstone」経路は、**記録ディレクトリが既に存在しないこと**だけを検証する。provenance が空なら承認すべき効果がそもそも無く、ディレクトリも無いなら壊すものが無いので、「何も指していない state を忘れる」ことは何も奪わない。usagi は所有権を証明できないディレクトリを自分では削除しないので、オペレータが確認して自分で消したあとに tombstone（と同名 record）を落とす、という順序でのみ意味を持つ（先に落とすと次の reconcile が再隔離するだけ）。これにより stray 隔離と ghost session（record はあるが provenance が空・ディレクトリも無い）の両方に出口ができる。メッセージで何を確認したことになるのかを明示する。
- `Release` は半分壊れたセッション（記録 worktree が欠けている）を拒否し、`--resume` を案内する。
- 読み取り専用の所有権検証 `reconcile::prove_live_session` を追加した。`discard_session` の preflight と違い、既に消えた worktree を冪等な部分 teardown として許容しない（半分壊れたセッションを「通常」へ戻さないため）。
- 記録された `force` は保たれる。未 force の teardown は未コミット変更を捨てず、その場合は隔離が解けているので通常の `session remove <name> --force` が使える。
- 削除ロックを呼び出し全体で保持し、store ロックより先に取る（`remove` と同じ順序）。

### 2. 到達可能な 2 surface

- **TUI コマンドパレット**: `session recover [workspace:]<name> --resume|--release`。2 つは逆向きの操作なので既定値は無く、未指定・両方指定・不明フラグ・名前欠落はすべて usage エラー。補完は subcommand・セッション名・2 つのフラグを出す。バックグラウンド worker（`TaskKind::RecoverSession`、ラベル「回復」）で走り、`--release` は pool eviction もサイドバー行の除去も行わない。
- **MCP tool**: `session_recover { name, action: "resume"|"release" }`。`action` は必須で enum。合成サーバ（`mcp/usagi.rs`）からもルーティングされる。

### 3. 隔離メッセージが次の一手を案内する

`removal_target` / `execute_teardown` の拒否メッセージは、由来に応じた回復操作を名指しする（`recovery_guidance`）。`Reconcile` 由来には `--resume` を案内しない。旧文言の「inspect state.json and `git worktree list`, then clean up the confirmed paths manually」は無くなった。

### 4. 維持したもの

`usagi clean` / `resume_pending_removals` は従来どおり `Orphaned` を skip する。隔離は**オペレータの明示指示があって初めて**動く（#470 の fail-closed）。

## 非対象

- teardown が branch 乖離で隔離してしまう不具合そのもの（#546、完了済み）。
- 隔離を自動的に解いて削除を進めること。
- store lock の短命化・中断 resume（#538）／MCP の逐次 dispatch（#539）／rename-to-trash（#541）。

## 受入条件

- [x] teardown 由来で隔離されたセッションが、`state.json` の手編集なしに回復操作だけで削除完遂できる。
- [x] 生きているのに誤って隔離されたセッションが、回復操作で通常のセッションへ戻り、以後の `remove` / 表示が通常どおり動く。
- [x] 所有権を再証明できない対象は隔離のまま残り、破壊的効果が一切出ない。
- [x] stray 隔離（provenance 空）が自動再証明の対象にならない。
- [x] 隔離時のエラーメッセージが、追加した回復操作を案内する。
- [x] 回復操作は TUI コマンドパレットと MCP tool の両方から呼べる。
- [x] `usagi clean` / `resume_pending_removals` は従来どおり `Orphaned` を skip する。

## 回帰テスト

`usecase/session/mod.rs`（`quarantine_as_teardown` / `record_a_ghost` fixture）:

- teardown 隔離 → `Resume` → 削除完遂（worktree / session tree / 記録 branch / record / tombstone がすべて片付き、同名 `create` が再び通る）。
- teardown 隔離 → `Release` → 通常セッションに戻り、`statuses` が `removal` なしで投影し、その後の `remove` が成功する。
- 再証明できない teardown 隔離への `Resume` が失敗し、session tree / branch / record が残り、`Orphaned` + `Teardown` に戻る。
- `Reconcile` 隔離への `Resume` が「再証明する材料が無い」と拒否し、`--release` を案内する。
- stray の `Release` がディレクトリ存在中は拒否し、削除後は成功して再 reconcile でも再隔離されない。
- ghost session（provenance 空・ディレクトリ無し）の `Release` が tombstone と record を落とし、同名 `create` が通る。ディレクトリが実在する場合は拒否し、その中身が残る。
- provenance 空の `Resume` 拒否、record 手削除済み tombstone の `Resume` 拒否。
- 無傷でないセッションの `Release` 拒否（`--resume` 案内）。
- 隔離でない pending（`git_teardown`）/ pending 無しへの回復拒否。
- `resume_pending_removals` が `Orphaned` を引き続き skip する。
- `Reconcile` 由来の拒否メッセージが `--resume` を案内しない。

surface:

- コマンドパースと補完（`command/tests.rs`）、パレットから recovery worker への routing（`event/tests/session_lifecycle.rs`）。
- MCP tool のスキーマ・両 action の routing・不正 action の拒否（backend に到達しない）・拒否の tool error 化、合成サーバ経由の routing。

## docs / 移行影響

`quarantine` フィールドは #546 で導入済み・後方互換読み取り済みで、本 issue は表現を追加しない。`quarantine` が `None` の legacy `orphaned` は、ラベルではなく tombstone の実体（provenance の有無・record の有無）で判定するため、最も保守的な側に落ちる。v1 の仕様ドキュメント（`v1/document/`）は退避版のため更新しない。
