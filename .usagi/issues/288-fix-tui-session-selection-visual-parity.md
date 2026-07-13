---
number: 288
title: "fix(tui): session の current/cursor 表示を v1 parity にする"
status: todo
priority: high
labels: [tui, bug, parity, sidebar, switch, closeup]
dependson: [258]
related: [225, 257, 269, 278, 279, 281, 282, 287]
created_at: 2026-07-13T00:00:00+00:00
updated_at: 2026-07-13T00:00:00+00:00
---

## 目的

v2 TUI の session 行で、現在開いている target（current / active）とキーボードで選択中の候補（cursor / selected）を、通常の左ペイン、Switch、Closeup のすべてで同時に判別できるようにする。v1 の gutter grammar と stable identity による復旧規則を、#258 が runtime の Home state を一元化した後の唯一の描画経路へ最小限で移植する。

## 調査根拠

- v1 の `sidebar.rs` は `selected_index`（keyboard cursor）と `active_index`（command target）を別に持つ。Overview/Switch では cursor が最優先で、session は先頭の one-cell rabbit `󰤇` と後続 `▎`、root と `+ new session` は `>` を描く。cursor が別行にある active session は緑の `▎` を行全体に描く。Closeup では cursor を消し、active の緑 `▎` だけを残す。したがって意味は色だけに依存しない。
- v1 は `activate_selected()` と list rebuild 時の `(workspace root, session name)` identity restore を分け、cursor と active が別でも、削除・更新後に存在しない target は安全に root へ縮退する。
- 現行 controller の `HomeProjection` は `Selection` と active target を stable `WorkspaceId` / `SessionId` で表せる一方、実 runtime は `presentation::Workspace` の別 state を描画している。旧 `menu_row()` は selected だけを `>` / accent にしており、current marker と mode-aware の優先順位を表せない。`replace_sessions()` も表示名で cursor を復旧するため same-name recreation を区別できない。
- #258 は root-first row contract と runtime/controller の統合を担当している。そこへ行列・state source の変更を重ねず、完了後の projection/renderer seam に marker contract を追加する。本 issue は #258 に依存する。
- #269 / #278 は Ctrl+A、Switch/Closeup の focus・close/prefix input contract を固定済みであり、本 issue は key dispatch の意味を変更しない。#257 / #287 の create lifecycle が landing を実装する際も、ここで定義する stable-id projection を消費する。

## 表示契約

| 状態 | Switch の session 行 | Closeup の session 行 | root / `+ new session` |
| --- | --- | --- | --- |
| cursor のみ | `󰤇` + `▎` stack、強調名、非 cursor 行より優先 | 到達不可（Closeup は cursor を表示しない） | `>` と強調 label |
| current のみ | 緑 `▎` を session 行の全行に表示 | 同じ緑 `▎` を保持 | 緑 `▎` |
| cursor = current | cursor 表示を優先し、current を二重 marker にしない | current 表示 | `>` を優先 |
| どちらでもない | gutter を空にし、通常表示 | gutter を空にし、通常表示 | gutter を空にし、通常表示 |

session の `󰤇` が使えない端末でも、継続 `▎` と name emphasis が cursor の意味を残す。全 marker、label、detail は ANSI を閉じた表示幅で clip/pad し、極小幅・CJK・長い session 名で行幅や後続 style を壊さない。pending create/remove skeleton は navigation target でも current でもなく、marker を奪わない。

## スコープ

- Home projection と renderer に `cursor` と `current` を別 stable identity で渡す共通 row presentation API を置く。index、表示名、tab label を identity にしない。
- 通常左ペイン、Switch、Closeup が同じ row presenter を使い、上表の marker precedence を描く。Closeup は current を維持しながら cursor emphasis だけを抑止する。
- snapshot refresh、session create success landing、delete、same-name recreation、一覧 reorder、mode switch/close、Closeup tab close の後に cursor/current を検証し、消えた stable ID は root へ安全に縮退する。background session の event は表示中 target・mode・cursor を奪わない。
- #257/#287 の Ctrl+A create form と #269/#278 の Switch/Closeup focus・close/prefix contract を regression で保護する。key binding、daemon request、pane/terminal lifecycle の仕様は変更しない。
- `document/03-tui.md` の Home sidebar 節を、実装と同じ PR で「cursor/current の意味・marker precedence・mode ごとの可視性・狭幅 clipping」の正本として更新する。

## 対象外

- #258 の runtime/controller 統合、root-first row migration、viewport algorithm を別実装で再度行わない。
- session create/remove の daemon lifecycle、inline form、Ctrl+A decode（#257/#287）、Closeup tab registry/terminal attach（#281/#282）の変更。
- v1 の branch status、agent lifecycle、manual label、mascot、multi-workspace sidebar 全般を v2 に先行移植すること。
- 色・glyph のテーマ再設計、mouse interaction、session 名による legacy pane-map の恒久的な再設計。

## 完了条件

- Switch で current session と cursor session が異なるとき、両者を文字/記号と強調で区別でき、同一なら cursor 表示が優先する。
- Closeup でも current marker は左ペインに残り、Switch 専用 cursor は残らない。mode transition と action/pane close は current/cursor の意味を崩さない。
- root、session、`+ new session`、empty list、pending skeleton、long/CJK label、極小幅で marker precedence と width safety が維持される。
- snapshot replacement、delete、create success、same-name recreation、reorder、stale/duplicate/background event の各ケースで stable identity を誤って再選択せず、必要なら root fallback になる。
- Ctrl+A、Switch/Closeup の prefix/focus/close 既存テストが通り、仕様を `document/03-tui.md` に実装済みの現在形で記録する。

## テスト

- pure state/reducer: cursor/current の別移動・activate、mode transition、snapshot/reorder、delete、create landing、same-name recreation、stale/duplicate/background event、root fallback。
- pure render/golden: 上表の 4 状態を normal/Switch/Closeup で、root/action/pending、ANSI reset、CJK と tiny geometry を含めて固定する。
- runtime integration: fake terminal で Switch → Closeup → Switch、tab/action close、Ctrl+A create entry/cancel/landing を通し、cursor/current の marker と input owner が既存契約どおりであることを確認する。
