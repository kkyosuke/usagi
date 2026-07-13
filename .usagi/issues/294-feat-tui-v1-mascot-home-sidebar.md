---
number: 294
title: feat(tui): v1 mascot の吹き出しと下部予約行を Home sidebar へ移植する
status: done
priority: high
labels: [tui, parity, mascot]
dependson: [258, 288]
related: [287, 276]
parent: 227
created_at: 2026-07-13T12:17:27.455514+00:00
updated_at: 2026-07-13T12:31:45.778831+00:00
---

## 背景

v1 の Home sidebar は、左下の 3 行 mascot の上に、実在する作業・更新通知を黄色の角丸吹き出しで出す。吹き出しは sidebar 幅で Unicode 表示幅を基準に wrap し、下枠の `┬` tail を mascot の頭へ向ける。吹き出しを出せない狭幅・空 message は静かな mascot に戻る。

現行 v2 の `workspace::home_mascot` は純粋な固定 3 行で、表示領域は mascot 分だけしか sidebar viewport から引かない。そのため吹き出しの可変行数、mascot 直下の 1 行予約、pending skeleton・root・`+ new session` と viewport の共通 geometry が未定義である。現行 daemon projection に update / task といった mascot 用メッセージ source は見当たらないため、文言を捏造してはならない。

## 目的

v1 を参照して、v2 Home 左下 mascot に安全な speech bubble surface を追加する。同時に mascot の直下に常に 1 行分の予約領域を置き、sidebar list / pending session / root / `+ new session` の可視行計算を mascot block の実高さと同期する。bubble は input target にせず、既存の Switch・Closeup・session selection・terminal tab の入力所有権を変えない。

## 実装方針

- #258 が runtime の唯一の `root → sessions → + new session` row contract を接続した後、その Home projection/render seam に実装する。#288 の fixed 2-line session entry と同じ sidebar vertical-layout helper を使い、entry 高さ・pending footprint・mascot reservation を二重定義しない。
- presentation boundary として `MascotSpeech`（表示済み安全な行列または空）と、art/bubble をまとめる `MascotBlock`（行列、animal body hit rect、総高さ）を置く。controller/daemon が source を供給できない間は `None` を渡して silent mascot を描く。renderer が update、progress、翻訳済みダミー文言を生成しない。
- 将来 source が接続される場合だけ、既存 event/snapshot の表示安全なメッセージを boundary へ渡す。priority・lifetime・acknowledgement・click action はこの issue で創作せず、source の所有者を別 issue で明確化する。
- bubble があるときは v1 と同じ黄色/太字の `╭─╮` / `│ text │` / `╰─┬─╯` chrome と tail を mascot 頭の列に固定する。content は ANSI-safe / Unicode display-width-safe に wrap と clip し、各 row は共通 block 幅へ pad する。最小幅を満たさない場合、または empty speech は破損した bubble を描かず silent mascot に fallback する。
- mascot を表示する場合は **art/bubble の直下に必ず 1 blank row** を確保する。sidebar body の available rows は footer、heading、root/session/new rows、pending skeleton、mascot block height、下部 blank rowを同じ geometry API から計算する。収まらない場合は list viewport を優先して mascot block 全体を省略する（reserved blank だけを独立表示しない）。
- mascot/bubble は decoration のみで focus、keyboard、mouse input owner を取らない。既存に mascot click/hit testing が無い v2 では click action を追加しない。将来 hit test を導入する場合も animal body のみを対象にし、bubble は対象外とする。
- resize、height 0/1、empty session list、pending create/remove、rail/narrow sidebar、CJK/combining/ANSI text を geometry の入力として扱い、overflow・underflow・style leak を起こさない。

## 受け入れ条件

- mascot が表示される全 frame で、mascot block 最終行の直下に 1 行の空行があり、その後に footer がある。bubble 表示時も同じ規約を保つ。
- short terminal、small sidebar viewport、empty session list、root、`+ new session`、pending create/remove、#288 の 2-line row で、viewport は選択行を含み、行が重複・欠落・scroll drift しない。
- message source が無い現行 runtime は bubble を出さず、dummy text を描かない。一方、presentation boundary は message が接続された場合に複数行・narrow-width bubble を純粋に描画できる。
- bubble は tail、border、padding、wrap、ANSI reset、Unicode display width を満たす。幅不足・empty text は silent mascot に安全に fallback する。
- Switch/Closeup の mode transition、selection/active、`+ new session`、terminal tab switching/closing、live terminal passthrough と focus owner は同一である。bubble は input を所有しない。
- resize ごとに geometry を再計算し、最小 geometry と clipping で panic、右端自動折返し、ANSI color bleed を起こさない。

## テスト

- widget/render: silent/bubble block の tail・padding・common width、空 message・最小幅 fallback、CJK/combining/ANSI wrap/clip/reset、tick の footprint 不変。
- layout: mascot 直下 blank row、bubble 高さ変化、footer 固定、2-line session row、pending skeleton、root/`+ new session`、empty list、short height/width、resize における viewport / selected-row visibility。
- reducer/runtime: message none が無言であること、将来 boundary の speech が input state を変えないこと、Switch/Closeup、selection、tab input passthrough が同じであること。
- visual parity: fake terminal frame または golden に v1 shape と narrow clipping を固定する。

## 依存・競合回避

- 依存: #258（runtime Home projection の一本化）、#288（2-line session row projection）。
- 関連: #287（`+ new session` action row）、#276（runtime Home chrome）。
- 主な変更先は `crates/tui/src/presentation/views/workspace.rs` と、必要なら mascot/widget 専用 module に限定する。session row の metadata projection、daemon wire、terminal input classifier、Closeup tab state は変更しない。
- 実装と同じ PR で、実装済み仕様だけを `document/03-tui.md` の Sidebar mascot に更新する。
