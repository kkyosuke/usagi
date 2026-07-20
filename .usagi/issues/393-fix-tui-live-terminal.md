---
number: 393
title: fix(tui): live terminal の通常クリックで保持済み選択を解除する
status: in-progress
priority: medium
labels: [tui, bug]
dependson: []
related: [390]
created_at: 2026-07-20T03:56:14.791768+00:00
updated_at: 2026-07-20T03:56:17.923593+00:00
---

## 概要

issue #390 により live terminal の drag 選択は release 後も保持される。保持済み選択を明示的に解除する通常左クリックが未実装で、右ペイン content の click は sidebar 用 event として扱われ inert になる。

## 方針

- 選択中の live terminal の content viewport 内での通常左クリックだけを shell 側で消費し、text selection を解除する。
- sidebar click（single / double）、modal / inline input の pointer ownership、PTY への入力非転送、drag/release copy の既存契約を変えない。
- content 外、selection 無し、overlay 表示中は既存処理へ委ねる。

## 完了条件

- release 後に残る terminal / agent text selection は右ペイン content の通常 click で解除される。
- sidebar の navigation / activation と modal input ownership は回帰しない。
- pure controls と shell pointer dispatch の回帰テスト、`document/03-tui.md` の選択契約更新を含む。
