---
number: 384
title: fix(tui): Switch セッション削除の「wave」loading（削除中インジケータ）が表示されない回帰を直す
status: todo
priority: high
labels: [tui, bug]
dependson: []
related: []
created_at: 2026-07-20T01:14:13.614950+00:00
updated_at: 2026-07-20T01:14:13.614950+00:00
---

# 概要

v2 TUI の Home サイドバー（`Switch` モード）で `x` / `X` によりセッションを削除するとき、削除中に出るはずの **「wave」loading インジケータ（Danger 赤の shimmer + `✂`）が表示されない**というユーザー報告の回帰。

ユーザー文: 「削除の際に出るUIが消えてる」→ 補足「wave の loading が消えてます」。特に **session 作成/一覧まわりを操作した後** に削除すると再現しやすい、という状況が示されている。

このタスクは削除の wave loading を**再表示・アニメーションし、途中で消えないよう堅牢化**し、回帰を検知できる **regression tests（reducer / render / runtime / fake daemon）** を追加することが目的。**削除の即時実行を勝手に走らせない**（確認・可視化が本題）。

# 削除 wave の仕組み（現状の連鎖）

`drive_workspace_controller`（`crates/tui/src/presentation/mod.rs`、本番経路。`src/runtime/tui.rs:1405` から呼ばれる）が使う **legacy shell 経路**が wave を描く。連鎖は以下:

1. `Switch` で `x`/`X` → reducer `remove_selected_session`（`controller.rs:2688`）が `state.move_selection(-1)` の後 `Effect::RemoveSession` を emit（**ここは tested。`controller.rs:4512`**）。
2. `dispatch_controller_effect`（`mod.rs:1626`）が `ui.removing_session = Some(session)` をセットし `begin_session_command`（`mod.rs:1034`）で worker thread を起動。
3. 毎フレーム `project_controller_sessions`（`mod.rs:1330`）が `projected.removing = ui.removing_session == Some(id)` をセット。
4. `home_row_lines_at`（`workspace.rs:1251-1266`）が `removing == true` の行に shimmer wave（`✂` + `shimmer_text_with`, Danger）を描く。アニメは `home.mascot_tick`（`AppEvent::Tick` でのみ加算）。
5. worker 完了時、`drain_session_completions`（`mod.rs:1094`）が `ui.removing_session = None` を**無条件で**クリア（`mod.rs:1098`）→ 行が消える。

> `PendingRow::Removing`（`usecase/application/lifecycle.rs`、tested）は別系統の daemon-backend state machine で、**本番の wave 描画はこちらではなく上記 legacy 経路**。混同しないこと。

# 根本原因の候補（すべて file:line 付き・いずれも「wave が出ない」を独立に説明できる）

**重要:** 2〜5 の presentation glue（dispatch / begin / drain / project / render）は**すべて `#[coverage(off)]` で regression test が無い**。だから回帰が検知されずに滑り込んだ。

### (A) ポートが busy のとき削除が黙って捨てられる（最有力・ブリーフの再現状況と一致）
`Effect::RemoveSession` の dispatch は `ui.session_commands.is_some()` で gate されている（`mod.rs:1627-1628`）。直前の session command（例: ユーザーが直前に行った **create**）の worker がまだポートを保持していると、**削除が黙って no-op**（wave も出ず削除もされない）になる。しかも reducer は既に `move_selection(-1)` でカーソルを動かしているため、「カーソルだけ動いて何も起きない」という不整合になる。ブリーフの「作成/一覧を操作した後」に一致。

### (B) 描画レースで wave フレームが 0 になりうる（最小可視時間の保証が無い）
`drain_session_completions` はループ先頭（`mod.rs:1942`）で走り、完了時に `removing_session` を**無条件クリア**（`mod.rs:1098`）してから次の `project`+`render` に進む。worker が次フレーム前に完了すると、**wave が一度も描かれない**。「wave が最低 1 フレームは描かれる」保証が無い。daemon の削除が速いほど wave が出なくなる（タイミング依存の flaky）。

### (C) #1102 の decision modal が `x` を塞ぐ
`x`/`X` は `state.overlay.is_none()` で gate（`controller.rs:2648`）。#1102（`c0418fe1`）はループ前（`mod.rs:1940`）と pending 到着時に **decision modal を自動 open** する。modal（overlay）が開いている間は削除キー自体が無効化され、wave も削除も起きない。

### (D) Tick 供給が途切れると wave が固まる
`mascot_tick` は `AppEvent::Tick` でのみ加算。wave のアニメは 16ms `EventPump` の Tick が blocking な `read_key` を起こし続けることに依存（`src/runtime/tui.rs:964-966` → `mod.rs:1223`）。background command がポートを持つ間に Tick 起床が止まると wave が固まる/描かれない。

# 推奨する修正方針

1. **(A) busy ポートを黙って捨てない**: `Effect::RemoveSession` 到着時にポートが busy なら、(i) 削除要求をキューして手前の command 完了後に実行する、または (ii) reducer 側のカーソル移動を巻き戻し／削除不可のフィードバックを出す。いずれにせよ「カーソルだけ動いて無反応」を無くす。単一ポート所有モデル（create/remove/refresh が同じ `session_commands` を共有）を壊さないこと。
2. **(B) wave の最低可視保証**: `removing_session` がセットされてから **最低 1 回は render される**まで `drain_session_completions` がクリアしないよう latch を入れる（例: `removing_rendered` フラグ。render 後に立て、drain はそれが立ってからクリア）。人工的な遅延は入れない。
3. **(C)** decision overlay が開いていても削除意図を失わない導線を検討（少なくとも「overlay 中は `x` が効かない」ことをフィードバックで明示）。**もし調査で「本当の再現は #1102 の overlay ブロック」だと確定したら本 issue から分離**してよい。
4. **(D)** background command 実行中も Tick 起床が続き wave がアニメすることを runtime test で保証。
5. **テスト可能化**: 2〜5 の判断ロジックを純関数へ抽出し `#[coverage(off)]` を外す。

# 追加する regression tests（ブリーフ指定）

- **reducer**: `x`/`X` が `RemoveSession` を emit し `force` が正しい（既存を維持）。root 行・`+ new session` 行・overlay 中は emit しない。
- **render**: `ProjectedSession.removing == true` の行が `✂` + shimmer wave 行を出す（現在テストゼロ。`workspace.rs:1251` の coverage-off を外す）。
- **runtime/shell**: `Effect::RemoveSession` dispatch で `removing_session` がセットされ、**完了前に最低 1 フレーム wave が描かれる**こと。ポート busy 時に要求が失われないこと。Tick でアニメが進むこと。
- **fake daemon**: fake `SessionCommandPort` で remove 成功/失敗、Esc cancel、reconnect/stale/duplicate を回帰させない。

# 壊してはいけない不変条件

- force(`X`) / normal(`x`) の区別、Esc cancel。
- 選択対象は stable な `SessionId`（daemon 名ではなく）で解決（`session_name_for`, `mod.rs:1053`）。
- root 行・`+ new session` 行は削除対象にしない。
- live pane input ownership（ポート所有・keystroke drop 処理）を壊さない。
- reconnect / stale snapshot / duplicate row を回帰させない。
- create の inline UI / 失敗 modal（#1090/#1091/#1097）を壊さない。

# 参考（コード位置）

- render: `crates/tui/src/presentation/views/workspace.rs:1215-1266`（wave）, `mod.rs:1330-1342`（project）
- dispatch/drain: `crates/tui/src/presentation/mod.rs:1094-1118, 1626-1639, 1034-1047, 1940-1942`
- reducer: `crates/tui/src/usecase/application/controller.rs:2647-2698`
- 別系統（混同注意）: `crates/tui/src/usecase/application/lifecycle.rs`（`PendingRow::Removing`）

# 調査の結論

削除 wave のコード自体は #1078 で導入され以後未変更だが、それを駆動/クリア/描画する presentation glue が全て untested（coverage-off）で、(A) busy ポートの silent drop・(B) 描画レース・(C) #1102 の overlay ブロックのいずれか（複数）が「wave が出ない」を引き起こす。まず (A)(B) を直し regression test を張るのが本命。docs（`document/` 該当箇所）も更新すること。
