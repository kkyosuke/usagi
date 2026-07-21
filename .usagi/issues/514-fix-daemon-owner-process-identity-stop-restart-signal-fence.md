---
number: 514
title: fix(daemon): owner process identity で stop/restart signal を fence する
status: in-progress
priority: high
labels: [review, v2, daemon, lifecycle, process-safety, security]
dependson: []
related: [160, 205, 209, 214, 350, 507, 513]
created_at: 2026-07-21T22:19:04.884487+00:00
updated_at: 2026-07-21T22:19:05.164436+00:00
---

## 問題・影響

shipping v2 の `DaemonRecord` は `pid + wall-clock started_at` だけを保存し、`stop` / `restart` は独立した `kill(pid, 0)` 後に `kill(pid, SIGTERM)` を行う。crash/stale record の PID が同一 UID の無関係 process に再利用されると誤 signal し、valid JSON の `pid = 0` は caller process group を対象にする。private data directory は cross-UID 改ざんを緩和するだけで、PID reuse、corruption、same-UID stale state、probe→signal TOCTOU を authority に変えない。

#209 は PID と process-start identity を照合し、PID reuse 時は signal しない契約を持つが、terminal 用 `ProcessIdentity` と shipping daemon lifecycle は未接続である。#507 の active/draining rollover と PR #1225 / #513 の planned-stop endpoint retirement・record clear ordering は別責務であり、本 issue はそれらを再実装しない。

## 修正方針

- daemon owner incarnation、検証可能な OS process-start identity、正の signal-safe PID を `DaemonRecord` に保存する。PID 0 / 1、負数、platform `pid_t` 範囲外、欠損・旧 schema・破損 identity は deserialize / registration 時に拒否する。
- PID-only liveness を authority から外す。start/status/stop/restart は完全な recorded identity を probe し、identity mismatch は stale、probe failure / unknown は fail closed として signal・spawn・record clear を行わない。
- stop/restart の termination は identity verification と effect を一つの capability に束ね、probe と raw `kill(pid)` の間に PID reuse 可能な窓を残さない。generation/owner/process-start identity を同じ Unix connection 上で照合して owner が self-shutdown する等、unrelated numeric PID を外部から signal しない方式を採る。raw PID fallback は置かない。
- 正規 planned stop だけが exact owner に shutdown を要求し、owner 側の cleanup/record clear/endpoint retire は #513 の fence を利用する。stale/forged record と identity unknown は無関係 process を変更せず、診断可能な failure を返す。
- restart は同じ exact-owner stop primitive 完了後だけ fresh start へ進む。#507 の将来 rollover もこの primitive を bypass しない。

## 受入条件

- [ ] PID 0 / 1、負表現、`pid_t` 範囲外、欠損/旧 schema、空・不正 owner/start identity を record 境界で拒否する。
- [ ] same PID / different process-start identity、owner incarnation mismatch、PID reuse、stale/forged record では stop/restart signal が 0 回で、unrelated process は生存する。
- [ ] identity probe failure / unknown と endpoint/peer identity mismatch は fail closed で、record clear・replacement spawn・raw PID fallback を行わない。
- [ ] probe 後かつ effect 前に identity が差し替わる TOCTOU でも unrelated process に signal を送らない。
- [ ] exact owner の planned stop は一度だけ shutdown し、#513 の quiesce → record clear → generation-fenced endpoint retire を完了する。
- [ ] restart は verified planned stop 後に一度だけ新 daemon を起動し、新 record は別 owner incarnation/process-start identity を持つ。
- [ ] start/status の alive 判定も PID-only authority を使わず、identity mismatch/unknown を安全に扱う。
- [ ] fake と実 Unix child/process integration の双方で no-signal と正規 stop/restart を固定する。

## 必須回帰テスト

- domain/store: PID boundary、identity validation、JSON schema/malformed/legacy record。
- usecase fake: exact match、gone、reuse/mismatch、probe error、effect-time mismatch、termination error、restart no-launch、start confirmation。
- Unix integration: signal-recorder unrelated child と実 daemon owner を別 process で起動し、forged/stale/PID-reuse相当 recordでは child に signal marker がなく生存すること、正規 stop/restart では owner だけが終了することを検証する。
- PR #1225 統合後の product lifecycle test で record owner identity と endpoint cleanup ordering を併せて確認する。

## docs / gate

`document/05-daemon.md` を process identity / fail-closed lifecycle の正本として更新する。Rust、durable schema、process/signal/Unix IPC に影響するため、fmt/check/clippy、推奨 selected tests、full test、coverage 100%、Markdown link check を必須とする。PR 前に最新 `origin/main` と #1225 を再取得し、merge 済みなら rebase、未 merge なら競合責務を重複実装せず統合可能になるまで待つ。
