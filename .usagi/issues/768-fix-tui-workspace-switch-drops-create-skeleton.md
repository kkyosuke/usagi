---
number: 768
title: fix(tui): workspace を切り替えて戻ると作成中 session の skeleton と完了通知が消える
status: todo
priority: medium
labels: [bug, tui]
dependson: []
related: [336, 384, 489]
created_at: 2026-09-18T00:00:00+00:00
updated_at: 2026-09-18T00:00:00+00:00
---

## 症状

Home サイドバーで session を作成し、**作成中 skeleton（wave）が出ている間に別 workspace（project tab / switcher）へ切り替えて元の workspace へ戻ると、skeleton が消えている**。作成そのものは daemon 側で進み続けるため、戻ったあとしばらくして **session 行だけが「急に」現れる**。

同時に次も失われる。

- 作成失敗時の失敗 dialog が出ず、黙って何も起きない
- 作成成功時の auto-landing（作成した session への focus）
- `+ new session` 直前の pending row という「作成中である」という唯一の表示

## 再現手順

1. workspace A の Home で `+ new session` → 名前を入力 → Enter（skeleton wave が出る）
2. skeleton が出ている間に project tab / switcher で workspace B へ切り替える
3. すぐ workspace A へ戻る
4. skeleton は無く、session も無い（作成中である手掛かりがゼロ）
5. しばらくすると session refresh で session 行が突然現れる

## 原因

1. skeleton の情報源は composition ローカルの `WorkspaceIoRuntime::creating_session` で、`WorkspaceIoRuntime::new` は常に `None` で構築する（`crates/tui/src/presentation/mod.rs`）。frame material も `ui.creating_session` をそのまま読む。
2. workspace 切り替えは `WorkspaceStep::Activate` を返して **composition を丸ごと teardown し、新しい workspace 用に再構築**する（`enter_workspace_deck`）。持ち越されるのは `WorkspaceSlot`（`path` / `workspace_id` / `label` / `sessions` / `focused_session` / `agents_observed`）だけで、pending create は含まれない。slot の doc comment 自身が "The controller is intentionally torn down on every project switch" と宣言している。
3. 切り替えを止める guard `workspace_has_unsaved_surface` は `create_session_form` / `note_editor` / `environment_editor` / `role_editor` という **draft だけ**を見ており、**in-flight の session command は見ていない**。よって作成中でも切り替えは素通りする。
4. create worker は `begin_session_command` の detached `std::thread::spawn` で、完了は `emit_session_command_result` と `sender.send` で **旧 composition の channel** へ返す。receiver は teardown で drop 済みなので、成功も失敗も **silently 捨てられる**（コード上も "If the workspace exited, the sink is closed harmlessly" と明記）。
5. 戻ってきた composition は `creating_session: None` なので skeleton を描かず、daemon の session refresh lane が次の snapshot を publish するまで何も出ない。これが「急にセッションが作成される」の正体。

つまり **skeleton が消えるのは teardown の副作用**であり、失われているのは表示だけでなく **完了・失敗の通知経路そのもの**である。#336 が没入（Attached）中の同型の取りこぼしを直したのと同じ欠落が、workspace 切り替え側に残っている。

## 修正方針

次の 3 案のいずれか。A は封じ込め、B / C が本質的な修正。

| 案 | 内容 | 長所 | 短所 |
|---|---|---|---|
| A | 切り替え guard に in-flight session command を含め、切り替えを拒否して notice（作成完了を待つ）を出す | 最小差分・取りこぼしゼロ | 作成中は切り替えを塞ぐ |
| B | pending create を `WorkspaceSlot` に持ち越し、戻ったときに skeleton を復元する | 表示は戻る | 完了検知を daemon の in-flight（同一 `OperationId` / revision）問い合わせへ載せ替える必要があり、復元した skeleton が永久に残らない保証が要る |
| C | create の完了受信を composition の外（deck / process レベルの lane）へ移し、切り替え中に完了した create の成功・失敗を戻ったときに適用する | 通知経路が切り替えから独立する。B の skeleton 復元も自然に乗る | 影響範囲が最も広い |

推奨は **A を先に入れて取りこぼしを止め、C を本命として別 issue に切る**。A だけでも「黙って消える」「失敗が見えない」は解消する。

## 受け入れ基準

- 作成中（`creating_session` が `Some`）に workspace 切り替えを試みると、切り替えが拒否されるか、または戻ったときに skeleton が復元される（採用案による）。
- 切り替えを挟んで作成が **失敗**した場合でも、ユーザーが失敗を知る経路が存在する（dialog または notice）。
- 切り替えを挟んで作成が **成功**した場合、session 行の出現が無通知の「突然」にならない。
- #336（没入中の完了適用）・#489（同時 2 件目の command）の契約を回帰させない。
- reducer / presentation の純関数側でテストを固定し、カバレッジ 100% を維持する。

## 参考

- `crates/tui/src/presentation/mod.rs`: `WorkspaceIoRuntime::new` / `begin_session_command` / `drain_session_completions` / `workspace_has_unsaved_surface` / `enter_workspace_deck`
- `crates/tui/src/presentation/workspace_deck.rs`: `WorkspaceSlot`
- `document/03-tui.md`: 作成中 skeleton の表示契約
