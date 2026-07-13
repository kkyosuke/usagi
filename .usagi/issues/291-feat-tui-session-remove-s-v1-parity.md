---
number: 291
title: feat(tui): session remove -s の選択モーダルを v1 parity で実装する
status: done
priority: high
labels: [tui, session, closeup, lifecycle]
dependson: [258]
related: [260, 286]
parent: 227
created_at: 2026-07-13T12:09:54.667337+00:00
updated_at: 2026-07-13T12:10:05.921633+00:00
---

## 背景

v1 は名前を指定しない `session remove` で中央の checklist modal を開き、`↑`/`↓`・`j`/`k`、Space、Enter、Esc で複数 session を選択・削除・取消できる。確認画面はなく、実行中は modal 内の各行を pending として保持し、完了結果でだけ行を更新する。

現行 v2 の Overview parser は `session remove [--force]` を現在の selected session の即時削除へ正規化するだけであり、selector state、modal renderer、入力 capture、複数の stable identity を安全に処理する effect runner がない。Closeup/Switch transition と snapshot reconciliation の変更が並行しているため、表示名や行 index による対象再解決を持ち込むと誤削除・stale focus の原因になる。

## 目的

Overview / Closeup から `session remove -s`（`--select` も同義）を実行すると、v1 相当の session 選択モーダルを開き、選んだ session だけを daemon-authoritative な削除経路で安全に削除できるようにする。既存の bare `session remove [--force]` は現在選択中 session を削除する意味を維持する。

## スコープ

- Overview と Closeup の command parser / help / completion に `session remove -s [--force]` と `session remove --select [--force]` を追加する。`-s` / `--select` と name・未知 flag・重複 flag の組合せは厳密に拒否し、bare remove の既存 semantics を変更しない。
- modal は snapshot を開いた時点の `SessionId` と表示用 label を entry として保持する。表示名、row index、path は削除 identity に使わない。current selected session も通常の候補として表示し、root は候補に含めない。
- v1 parity の keyboard contract を実装する: `↑`/`↓`・`j`/`k` は wrap、Space は toggle、Enter は選択済みだけを削除、Esc は未実行の選択を破棄して元の mode / focus へ戻す。空一覧、未選択 Enter、removing 中の再 submit は no-op か安全な feedback とし、追加の確認 step は設けない。
- 一括削除は selector が確定した stable ID の集合だけを daemon port へ渡す。port が 1 本であることを前提に、worker / completion で順序を管理し、pending ID、成功、失敗、port error、snapshot refresh を reducer に還元する。成功した row だけを落とし、失敗 / 未送信対象は modal に残して再試行可能にする。
- daemon snapshot が modal 表示中または削除中に対象を失った場合は、その stable ID を dispatch せず stale row / check / pending を安全に取り除く。snapshot の reorder、同名再作成、duplicate / delayed completion は別 incarnation を削除せず、現在の session が消えた selection / active / Closeup pane は root fallback と既存 pane cleanup に従う。
- successful completion では sidebar projection、Switch / Closeup の title・action focus・pane registry を同一 snapshot に同期する。modal を閉じる条件、失敗時の focus、Closeup から開いた場合の復帰先を明文化し、背景の input と live pane passthrough を modal が必ず遮断する。
- 実装済みの仕様だけを `document/03-tui.md` と command の正本へ更新する。

## 対象外

- daemon lifecycle / IPC wire、git/worktree 削除の再実装、直接 filesystem fallback。
- bare `session remove` の current-session semantics の変更。
- v1 の unite 複数 workspace selector、mouse 操作、新規の確認ダイアログ。
- #258 の root-first runtime 統合、#288 群の row visual redesign を同一変更で再実装すること。

## 受け入れ条件

- Switch または Closeup の Overview command から `session remove -s` を開くと、v1 に沿った中央 checklist modal が前面に出て、背景・Closeup action・live pane は入力を受けない。
- keyboard 操作で複数の session を選択し、Enter 後は選択した stable `SessionId` だけが remove request になる。current selected session を含んでも誤った隣接行や root を削除しない。
- Esc、空一覧、未選択 Enter、double Enter、daemon error、対象消失、snapshot reorder / 同名再作成、delayed / duplicate completion は panic・誤削除・stale selection を起こさない。
- 成功・部分失敗・全失敗の各場合で sidebar、選択 / active target、Closeup label / action focus、pane state が authoritative snapshot と一致して復旧する。
- v1 に確認ステップが無いことに合わせ、selector の Enter は選択済み削除を直接開始する。dirty session の拒否 / `--force` は daemon の既存 policy をそのまま用いる。

## テスト

- parser / command registry: `-s`、`--select`、`--force` の許可組合せ、bare remove 維持、invalid / duplicate / name 混在、Overview と Closeup の dispatch。
- pure modal reducer / render: empty、wrap、toggle、unselected Enter、Esc、pending lock、success / failure / partial success、narrow geometry。
- runtime fake port: multiple stable IDs、current session を含む削除、snapshot reorder、target disappearance、same-name recreation、stale / duplicate completion、daemon failure。
- integration regression: Switch → selector → remove → Switch、Closeup → selector → remove → Closeup / root fallback、modal 中の live input 遮断と pane cleanup。

## 依存・境界

#258 完了後の runtime で root-first row / stable target projection を利用する。#260 の daemon-authoritative session port と #286 の Switch / Closeup transition helper を再利用し、これらの既存 semantics は変更しない。
