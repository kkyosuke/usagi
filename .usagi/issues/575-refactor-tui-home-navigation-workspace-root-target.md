---
number: 575
title: refactor(tui): Home navigation から workspace root target を分離する
status: done
priority: high
labels: [v2, tui, refactor, navigation]
dependson: []
related: [388, 506, 510, 545]
parent: 571
created_at: 2026-07-27T23:03:12.865600+00:00
updated_at: 2026-07-28T00:35:12.002447+00:00
---

## 背景

Epic #571 では workspace root（現 UI の `main`）を managed session と同列に扱わず、別の Workspace Agent drawer へ移す。現行 `AppState` / `HomeProjection` は `Target::Root` を sidebar の先頭行、selected / active の fallback、通常 Closeup の target として共有しているため、表示上 `main` を消すだけでは hidden root action が残る。

本 issue は drawer を作る前提となる **managed-session navigation の状態モデルと sidebar** だけを所有する。

## 対象責務

- Home の selectable rows を `session* → + new session` に変更し、`main` row と root 直後の divider を削除する。
- 通常 Home の cursor / active managed session を `Target::Root` に依存しない状態へ分離する。`Option<SessionId>` または同等の型で「active session なし」を明示し、root を hidden fallback にしない。
- session 0 件では `+ new session` を選択し、Enter / Closeup / `terminal` / `agent` / `diff` が workspace root に流れないよう reducer/effect 境界で拒否する。
- snapshot refresh・session removal で selected / active session が消えた場合、表示順上の surviving session、なければ `+ new session` へ決定的に着地する。
- session create 成功時の auto-landing、remove、double-click、keyboard wrap、viewport scroll、pending skeleton、mascot reservation、pointer hit-test を root row のない geometry に合わせる。
- `Target::Root` と `session_id: None` は daemon/root Agent scope の語彙として残し、core / IPC contract は変更しない。ただし managed-session Closeup の public entry からは生成しない。
- Overview/config/env/decision の workspace-global scopeは維持し、managed session target の有無と混同しない。

## 非対象

- Workspace Agent drawer の描画・header button・`Ctrl-O g`（#576）。
- root Agent inventory / tab intent / attach / resume（#577）。
- New Agent CLI picker と launch（#578）。
- daemon/core の root scope 廃止。

未リリース機能のため、この issue が単独で landed した期間に旧 root Closeup へ到達できなくなることへの後方互換は不要である。root Agent の新しい入口は後続 issue が追加する。

## 受入条件

- [ ] sidebar の render、keyboard rows、pointer hit-test に `main` / root row / root divider が存在しない。
- [ ] 0 / 1 / N session で選択 wrap、viewport、footer、mascot、pending create の高さが一致する。
- [ ] session 0 件で `+ new session` 以外の通常 target がなく、root Agent / Terminal / Diff effect が発行されない。
- [ ] active session の削除・refresh・failed/deleting lifecycle で stale `SessionId` を実行に使わず、決定的な surviving row または New へ着地する。
- [ ] session create 成功後の interaction fence と auto-landing、remove、double-click が stable `SessionId` のまま動く。
- [ ] workspace-global Overview/config/env/decision と Welcome/quit の挙動に回帰がない。

## 必須テスト

- controller: 0/1/N rows、初期状態、上下 wrap、create/remove/refresh reconciliation、stale click、root action effect-zero。
- presentation: root 行なしの golden/line projection、狭幅/CJK、scroll、pending skeleton、footer/mascot。
- runtime: pane registry に root を active fallback として選ばないこと、managed foreground の維持。
- 変更時は `document/03-tui.md` を最終仕様へ先行更新せず、実装済み範囲だけを記載する。全体仕様の確定更新は #579 が所有する。
