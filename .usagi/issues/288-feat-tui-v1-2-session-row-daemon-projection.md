---
number: 288
title: feat(tui): v1 風の2行 session row を daemon projection へ接続する
status: done
priority: high
labels: [tui, session, parity]
dependson: [258]
related: [276, 287, 243]
parent: 227
created_at: 2026-07-13T12:02:51.086273+00:00
updated_at: 2026-07-13T12:17:13.116680+00:00
---

## 背景

v1 の Home sidebar は session ごとに名前・メモ、活動時刻、差分、PR を視認できる行を描画する。現行 v2 runtime は `root → sessions → + new session` の row contract と selected / active marker を持つが、`ProjectedSession` は `label` と `detail` だけで、左ペインは session 名と origin の1行表示である。

調査の結果、v2 の core `SessionRecord` には `last_active_or_created()`、`notes`、`prs` があり、PR modal も `PrLink` を表示できる。一方、これらは daemon snapshot / `SessionRow` / `ProjectedSession` にまだ投影されていない。diff command は `not_implemented` で、GIF の保存・状態モデル・導線はいずれも存在しない。未提供データを推測してバッジや操作を捏造しない必要がある。

## 目的

左ペインの実 runtime row を v1 の情報密度に近い、固定2行の session row に更新する。1行目は session name と note affordance、2行目は last modified と利用可能な diff / PR の状態または既存操作への導線を表示する。狭幅、empty metadata、selected / active / disabled / focus を既存の Switch・session selection contract のまま読み取れるようにする。

## スコープ

- #258 の runtime 唯一の `root → sessions → + new session` projection を前提に、session entry だけを2行の表示 contract にする。root と create action row の順序・identity・keyboard semantics は変えない。
- daemon snapshot から stable `SessionId` ごとに、表示安全な last modified（`last_active` が無い旧 metadata は `created_at` に fallback）、note の有無、visible PR summary を projection へ渡す。metadata / record が無い場合は root への安全な reconciliation と空表示を保つ。
- 1行目は選択 cursor / active marker、名前、note icon を優先して幅を確保する。note がない場合は icon を非活性表示または予約し、記号の有無だけで行が横に跳ねないようにする。note 操作が既存 overlay に接続済みならその導線を使い、未接続なら誤操作を誘う click/shortcut を追加しない。
- 2行目は相対時刻または安全な時刻 fallback と、既存 closeup / overlay に接続済みの PR summary を表示する。PR が無い・dismissed のみ・未解決 title のときは同じ行高で安全に縮退する。
- diff / GIF は daemon projection と実行可能な command が存在する場合だけ状態または導線を描く。現状は diff command が未実装で GIF データも無いため、この issue では unavailable をデータのように装わず、必要なら控えめな既存 Closeup action の案内に留める。新たな git probe、GIF capture、browser launch、local fallback は含めない。
- narrow geometry では名前・cursor/active・note を優先し、補足を ANSI-safe clipping する。2行1 entry を前提に viewport、pending skeleton、remove、mascot の予約行数と hit test を同期させる。
- Switch / Closeup の input owner、上下選択、Enter target、#287 の `+ new session`、live pane passthrough、disabled / hover / focus の既存 controller contract を変更しない。

## 受け入れ条件

- session row は通常時に2行で、1行目に name と note status、2行目に last modified と利用可能な PR / diff status-or-affordance が表示される。
- snapshot が `last_active`、note、PR を持つ場合、対応する値が stable session identity を経由して描画される。`last_active` / note / PR が無い旧 record、空 session list、snapshot 欠損は panic・stale target・幅崩れを起こさない。
- diff / GIF のデータ供給が無い現状では、存在しない状態、実行不能な shortcut、疑似的な progress 表示を追加しない。後続で実装可能な最小 projection/affordance boundary を明文化する。
- 選択、active、disabled/pending、keyboard focus の区別は狭幅でも読め、行の clipping は ANSI reset と Unicode display width を保つ。
- `+ new session`、root、pending create/remove、viewport、Switch の上下/Enter、Closeup と live-pane input owner の既存挙動は回帰しない。

## テスト

- pure projection/render: populated metadata、legacy/missing metadata、PR なし・dismissed・未解決、note あり/なし、狭幅/CJK/ANSI clip、selected と active の別 marker、disabled/pending skeleton の2行 footprint。
- reducer/adapter: stable `SessionId` で snapshot refresh 後も selection/active を保ち、欠損時は root に安全に縮退すること。
- runtime parity: fake daemon + terminal で Switch の選択・Enter・`+ new session` と Closeup/live-pane ownership を2行 row でも維持すること。

## 依存・境界

- #258（runtime の root-first row contract）の後に実装する。#287（create action row）とは表示領域を共有するが、作成フローは再実装しない。
- v1 参照は `v1/document/design/home/03-sidebar.md` と `v1/src/presentation/tui/home/ui/sidebar.rs`。v2 の現行仕様は `document/03-tui.md` を実装と同じ PR で更新する。
- diff/GIF のデータモデル・収集・実装可能な実行経路が必要になった場合は別 issue とし、この UI issue に隠れた IO を入れない。
