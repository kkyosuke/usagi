---
number: 75
title: ホーム画面の左セッションサイドバーを Ctrl-B で開閉できるようにする
status: done
priority: medium
labels: [feature, tui]
dependson: []
related: []
created_at: 2026-06-21T06:35:04.064101+00:00
updated_at: 2026-07-04T04:22:31.746570+00:00
---

## 目的

ホーム画面の左セッション一覧を畳んで右ペイン（特に没入の埋め込みターミナル）に幅を譲りたい。畳んでいても「どのセッションがアクティブか・各セッションが何をしているか」は分かるようにする。

## 仕様（実装済み）

- 左サイドバーを **フル幅 ⇄ レール（幅5桁）** の 2 状態でトグルする。
  - 非モーダルなホーム（統括 / 切替 / 在席）では `Ctrl-B` で直接トグル（`domain::settings::Sidebar` / `HomeState::toggle_sidebar`）。
  - **没入（埋め込みターミナル）中は `Ctrl-B` はシェル(PTY)へ透過**させ、サイドバー開閉は `Ctrl-O s`（prefix）/ `Alt-s` の予約キー（`Reserved::ToggleSidebar`）に割り当てる。
- レールは 1 セッション 1 行で、左端のアクティブバー `▎`・1始まりの連番・状態グリフ（`▶`/`◆`/`☾`/`✓`、Agent が無ければ種別ドット `●`/`○`）と git 状態グリフを表示。
- タイトルバーに常にアクティブセッション名を表示（単一ワークスペース時 `<workspace> · ▸ <active> · N session(s)`、unite 時は `unite · ▸ … · N sessions across G workspaces`）。
- 設定 `sidebar`（`full` / `rail`）で開く初期状態を制御（既定 `full`）。

## 補足

当初仕様の「切替(Switch)は常にフル幅で描画」「没入では Ctrl-B を横取り」は、実装では上記のとおり変更した（切替でもレールに畳める／没入は `Ctrl-O s`・`Alt-s`）。本文は実装に合わせて更新済み。`document/design/05-home.md` / `document/05-settings.md` / `document/data/01-global.md` を更新。
