---
number: 248
title: feat(daemon): generic TerminalObservation を raw PTY output から配信する
status: in-progress
priority: high
labels: [daemon, ipc, terminal]
dependson: [218, 220]
related: []
parent: 213
created_at: 2026-07-12T21:45:36.696689+00:00
updated_at: 2026-07-12T21:46:27.866462+00:00
---

## 目的

daemon が所有する PTY の raw output から、surface-neutral な `TerminalObservation` / `TerminalAnnotation` を生成・journal・replay する。最初の kind は URL とし、TUI の画面 scanner や Agent lifecycle hook、v1 の PR link harvest を正本にしない。

設計根拠は [IPC protocol](../../document/proposals/03-ipc-protocol.md)、[daemon API](../../document/proposals/04-daemon-api.md)、[daemon lifecycle](../../document/proposals/05-daemon-lifecycle.md) および v1 terminal link harvest である。v1 `TerminalPool` の移植ではなく、既存 v2 terminal actor/registry と IPC client reducer に責務を分ける。

## 対象

- `usagi-core` に closed enum の `TerminalObservationKind` と wire/reducer 語彙を置く。初期 variant `Url` は `TerminalRef`、terminal stream epoch、raw output の half-open byte range（`start_offset..end_offset`）、観測 URL value と必要なら canonical value を持つ。将来 variant は file reference / diagnostic / progress を予定するが、この issue で実装しない。
- daemon terminal actor が PTY raw bytes を journal へ commit した後、ANSI escape と invalid encoding を安全に扱う bounded incremental URL scanner を動かし、発見した annotation を対応 `TerminalOutput` の後に同一 terminal stream で commit・配信する。
- stream output offset と subscription event sequence を別軸のまま保持する。annotation は output byte range を参照し、event sequence を range/cursor に流用しない。
- `TerminalSnapshot` と bounded observation journal/cursor を拡張し、attach/reconnect/resume/resync で output と observation の欠落・重複を reducer が安全に扱えるようにする。history eviction、scrollback/prune、cursor が古すぎる・未来・epoch 不一致時は atomic snapshot/resync に収束させる。
- daemon の backpressure/resource limit に observation journal、URL 長、1 output あたりの発見数、pending scanner tail を加える。URL spam は bounded に drop/coalesce し、protocol frame を肥大化させない。URL は自動で開かない。
- TUI は generic annotation の reducer/projection を受け取るだけにし、表示は URL 下線・クリック等の generic projection に限定する。PR badge、`PrObserved` / `PrEnriched`、`gh` / GitHub 問い合わせ、title/state enrichment は導入しない。
- 実装済み契約を v2 の正本 document（IPC protocol / daemon API / architecture）へ移す。未実装の拡張は proposal にだけ残す。

## 受け入れ条件

- raw `TerminalOutput` を先に commit/deliver し、その output range に対応する `TerminalObservation` を後続 event として deliver する。same-range の URL 複数件は stable scan order を持つ。
- fresh attach、retained replay、disconnect/reconnect、duplicate event、event sequence gap、output offset gap、resync、epoch rollover、journal eviction、scrollback prune を pure reducer と fake transport で検証し、URL が消失・二重適用・旧 epoch 誤適用しない。
- scanner は output chunk 境界をまたぐ URL、ANSI CSI/OSC、UTF-8 split/invalid byte、末尾句読点、長すぎる URL、多数 URL を bounded memory/time で扱う。raw output range は元 bytes の offset を指す。
- daemon PTY integration で daemon source の raw output が observation として配信されること、socket integration で output-before-observation / replay / resync を確認する。
- `TerminalSnapshot` と observation cursor/journal の retention・resync boundary を API 文書に明記し、output offset と event sequence の混同を回帰テストで防ぐ。
- `cargo fmt --all -- --check`、`cargo check --workspace --all-targets`、変更 crate の test、`cargo clippy --workspace --all-targets -- -D warnings`、full coverage 100%、Markdown link check を通す。

## 非対象

- PR 専用 event/type、PR badge、URL の自動 open、`gh` / GitHub API/CLI、PR title/state enrichment。
- Agent hook を observation source にすること。
- v1 `TerminalPool` / PR link store の移植。
- file reference / diagnostic / progress variant の実装。
