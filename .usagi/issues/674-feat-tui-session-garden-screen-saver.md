---
number: 674
title: feat(tui): 無操作時に session garden screen saver を表示する
status: done
priority: medium
labels: [v2, tui, uiux]
dependson: []
related: [687, 688]
created_at: 2026-08-13T10:19:44.691141+00:00
updated_at: 2026-08-16T22:39:03.168497+00:00
---

## 背景

Home には sidebar mascot があるが、複数 session の状態を usagi らしい空間表現で俯瞰する面はない。一定時間操作がないとき、各 session を庭にいるうさぎとして表示する screen saver を追加する。

設計の正本は `document/proposals/15-session-garden.md` とする。

## スコープ

- Home の Switch / Closeup と daemon-owned pane を維持したまま、無操作 5 分で全幅 Garden layer を表示する。
- key、paste、mouse button、wheel、terminal resize を user activity とし、monotonic clock で idle time を判定する。
- tick、backend event、Agent/terminal output は idle deadline を延長しない。
- 確認 modal、編集中 form、Director drawer が前面にある間は Garden を開かない。
- lifecycle と Agent phase を、running / waiting / idle / creating / failed / deleting の決定的な usagi pose に投影する。
- stable `SessionId` と session 順から plot、animation phase、click hitbox を決める。ランダム配置や物理 simulation は使わない。
- うさぎの single click は対応 session を active / selected にして既存 Closeup へ遷移する。
- うさぎ以外の click、任意の key / paste / wheel は入力を消費して表示前の Home へ戻る。wake-up 入力を背面 terminal へ転送しない。
- 高さ 14 行未満または幅 64 桁未満では Garden を表示せず、既存 Home を保つ。
- Garden 専用 timer/thread や daemon schema / IPC / 永続 record の追加は行わない。

## 実装方針

- `crates/tui/src/presentation/widgets/garden.rs` に ANSI-safe / width-safe な純粋 renderer と `SessionId` 付き hitbox layout を置く。
- interactive frame loop が monotonic time と user input を観測し、時刻を注入済みの idle/wake event を controller へ渡す。controller 内で wall clock を読まない。
- controller は Garden の表示 lifecycle、wake-up の入力消費、stale session click の拒否、既存 Closeup 遷移を所有する。
- Garden は既存 frame tick と canonical pose を共有し、同じ描画 material では redraw を発生させない。
- 実装後に現在仕様を `document/03-tui.md` へ反映し、提案書は採用済み判断へ縮約する。

## 受け入れ条件

- 同じ session snapshot / tick / terminal size は byte-for-byte 同じ frame と hitbox を返す。
- すべての行が端末幅以内で、CJK session label も scalar / 表示幅境界で壊れない。
- 0 / 1 / 表示上限超過の session、全 lifecycle、narrow / short terminal を unit test で検証する。
- 注入した経過時間により、5 分未満では表示せず、5 分到達時に eligible な Home だけで表示することを検証する。
- backend event と terminal output は deadline を延長せず、user input と resize は延長する。
- 最初の wake-up 入力は背面へ伝播しない。うさぎ click だけが対応する既存 Closeup へ遷移する。
- click 対象が snapshot から消えた場合は daemon effect を発行せず Garden を閉じる。
- Garden 表示中も daemon-owned terminal / Agent と背景 observation lane は継続する。
- screen graph test で自動表示、wake-up、click-to-Closeup、元 route への復帰を固定する。

## 完了状況

このスコープは完了した。

1. 純粋 renderer と `SessionId` 付き hitbox（widget / unit test）
2. Overview の `garden` command による手動表示と wake-up
3. 自動表示（`AppEvent::IdleElapsed`）と click 遷移（`AppEvent::GardenClick`）、`document/03-tui.md#session-garden` への仕様反映

残る見た目の作業は別 issue へ切り出した。

- #687 — うさぎを agent 単位で描く（集約 phase をやめ、羽数・並び順・`+N` の畳み込みを追加する）
- #688 — 残りの pose・`USAGI_REDUCE_MOTION` の production 配線・選択強調・`Failed` の safe failure summary
