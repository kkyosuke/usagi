---
number: 249
title: feat(daemon): terminal observation から PR projection と更新 hook を発火する
status: done
priority: high
labels: [daemon, terminal, pr]
dependson: [248]
related: []
parent: 213
created_at: 2026-07-12T21:53:55.361643+00:00
updated_at: 2026-07-12T21:54:27.305557+00:00
---

## 目的

generic `TerminalObservation` の `Url` を daemon 内部で消費し、PR URL を durable な PR projection へ畳み込む。projection の commit が成功した後だけ、更新内容を表す daemon hook を発火する。`TerminalObservation` 自体を TUI へ配信することは要件にしない。

この hook は Agent lifecycle hook ではない。terminal actor からの raw output 観測を起点に、daemon が状態を更新したことを伝える post-commit notification である。

## 対象

- daemon の observation consumer が `Url` を PR URL classifier へ渡し、canonical PR identity（少なくとも canonical URL と repository / number を抽出できる場合はその値）で durable projection を upsert / dedupe する。source `TerminalRef`、stream epoch、raw output range と observation cursor を provenance として保存し、同じ観測の replay で重複追加しない。
- projection の mutation と processed-observation cursor / dedupe key を同じ durable commit にする。crash、reconnect、journal replay、duplicate hook delivery で PR state が巻き戻らず、更新を失わない。
- `PrProjectionUpdated` のような post-commit hook port を daemon usecase に置く。hook payload は projection revision、更新対象、added / changed / unchanged の結果、source observation identity を持つ。実際に durable change が無い duplicate では update hook を新規発火しない。
- hook の delivery は少なくとも at-least-once を前提に consumer が revision / observation identity で dedupe できる形にし、hook failure が committed PR projection を rollback しない。reconnect 先または将来 subscriber が cursor gap を検出した場合は projection snapshot / journal から resync する。
- TUI は terminal annotation stream を購読しない。将来 PR modal を実装する場合は、この daemon-owned PR projection / update hook を入力にして表示する。TUI 不在でも projection と hook journal は daemon 側で進む。
- URL を自動で開かない。外部 `gh` / GitHub 問い合わせ、title/state enrichment は projection update を block しない別 adapter / 別 issue とし、この issue では canonical URL 由来の情報だけを正本とする。

## 受け入れ条件

- TUI 未接続の daemon-owned PTY output から PR URL を検知し、daemon の PR projection が更新される。
- 同一 URL、canonical 化前後の同一 PR URL、output / observation replay、daemon crash 後の再実行で重複 PR を作らない。
- projection commit 後にだけ hook を発火し、hook callback / transport failure 後の retry が projection を二重更新しない。
- malformed URL、非 PR URL、長すぎる URL、observation journal eviction、epoch mismatch、cursor gap を fail-closed / resync-safe に扱う。
- pure classifier / projection reducer / hook delivery policy、fake store / hook、socket または daemon actor integration をテストする。TUI が terminal observation event を受け取らなくても PR state が更新されることを確認する。
- 実装済みの IPC / daemon API / architecture document を更新する。

## 非対象

- `TerminalObservation` を PR 固有 type へ置き換えること、または TUI への terminal annotation 配信。
- URL の自動 open。
- `gh` / GitHub API による PR title / state enrichment、または Agent lifecycle hook の再利用。
