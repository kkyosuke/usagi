---
number: 350
title: feat(daemon): daemon/agent を restart・crash・macOS 再起動後も durable に復旧する
status: todo
priority: high
labels: [daemon, launchd, session, agent, recovery, resilience, security]
dependson: [349]
related: [209, 221, 254, 310, 311, 348, 253, 252, 250]
created_at: 2026-07-18T01:53:34.283021+00:00
updated_at: 2026-07-18T01:53:34.283021+00:00
---

## 背景・根拠

現状、`usagi daemon start` は detached な `serve` を起動して `daemon.json` に pid を登録するだけで、crash・kill・電源断・macOS 再起動/login をまたいで daemon を自動的に立て直す supervisor が存在しない。daemon が落ちると:

- managed session の durable state（`<data-dir>/daemon/sessions.json`）は残るが、daemon process と Agent PTY は失われる。#209 の orphan 契約どおり PTY master fd は復元不能で、生存 child は `orphan_running` / `identity_unknown` として attach/replacement を拒否する（daemon crash 後の PTY 継続自体は #221 の将来設計であり本 issue の対象外）。
- sidebar から見た session の Agent は「消える／不定」になり、利用者は Claude/Codex の作業文脈へ戻る導線を失う。

一方で、既存の関連作業は本 issue と役割が異なる:

- **#348 / #1040**: shared `sessions.json` が **未作成の最初の起動**に限り、検証済み legacy `state.json` を available managed session として一度だけ採用する。
- **#349**: 既に partial な v2 lifecycle state（例: failed `test-1`）を持つ利用者向けに、`usagi session recover-legacy`（daemon IPC `SessionAction::RecoverLegacy`、dry-run 既定 + `--apply`）で legacy session を明示採用する **operator recovery 経路**。**＝「既存 partial state の復旧」はこの #349 が担当する。本 issue はそこへ依存し、同じ検証・atomic writer 契約を再利用するが復旧経路を作り直さない。**
- **#254**: daemon 生存期間内での adapter resume/reclaim（verified identity のみ、ambiguous は fail-closed）。
- **#310 / #311**: restart 時の trusted root cwd 統一と durable atomic JSON writer。

本 issue が埋めるのは、これらの外側にある「**daemon process と Agent process が完全に消えた後**（restart / crash / SIGKILL / 電源断 / macOS 再起動・login）でも、daemon を supervisor で立て直し、session を interrupted として可視化し、provider ネイティブの resume 情報を残して**利用者の明示操作で** Claude/Codex を再開できる」という resilience 契約である。

## 目的

1. **launchd supervision**: macOS で launchd が `usagi daemon serve`（detached `start` ではなく前景 serve）を supervise し、crash・login・再起動後に単一インスタンスとして再起動する。daemon 単一インスタンスの権威は従来どおり `serve` が保持する `daemon.lock` に置き、launchd は process supervisor に徹する。
2. **interrupted 可視化**: daemon 起動時の reconcile で、durable state 上は `available` だが Agent runtime が失われた session を「中断（interrupted, resumable）」として projection し、sidebar から消さない。lifecycle の closed vocabulary（`creating`/`initializing`/`available`/`deleting`/`failed`）は変更せず、Agent runtime liveness という別軸で表現する。
3. **provider resume metadata の永続化**: `agents.json` に、明示 Resume に十分なだけの provider metadata（provider 種別 Claude/Codex、worktree/cwd identity、安定した provider-native session id/name、last-known status/phase、adapter revision）を durable に保存する。secret・argv・transcript 本文は保存しない。
4. **明示 Resume**: 利用者の明示操作からのみ `claude --resume <session>` / `codex resume <session>` に相当する新規 Agent を、解決済み managed-session worktree で起動する経路（daemon IPC `SessionAction::ResumeAgent`、CLI、TUI、MCP）を提供する。**既定で自動継続はしない。**

## 非目標（明示的に out of scope）

- **Agent の自動継続（auto-resume / auto-continue）**。crash 前の作業を無人で再開すると、危険な／陳腐化した操作（破壊的コマンド、古い前提に基づく編集、二重の外部副作用）を replay しうるため、既定で行わない。将来 opt-in にする場合は別 issue とし、per-session の明示同意・冪等性保証・verification gate を前提とする。本 issue では auto-resume を実装しない。
- **daemon crash 後の PTY 画面・入出力の継続 attach**（broker / FD handoff）。#221 の将来設計に委ね、本 issue は #209 の explicit orphan 契約を維持する（生存 child は kill/attach/replacement せず interrupted として表示）。
- 既存 partial v2 state / legacy `state.json` の採用ロジック（#348 / #349 が正本）。本 issue はその結果を projection・resume するだけで、採用・検証・atomic commit を再実装しない。
- Windows / Linux の service supervision。まず macOS の launchd を対象とし、他 OS 対応は将来 issue とする（本 issue は非 launchd 環境で従来の `usagi daemon start` 経路を壊さないことだけを保証する）。

## アーキテクチャ（所有境界）

| 層 | 所有する責務 |
|---|---|
| launchd（LaunchAgent） | `usagi daemon serve` の process supervision。`RunAtLoad`（login/再起動時起動）と `KeepAlive`（異常終了時再起動）。managed state を解釈しない。単一インスタンスの権威は持たず、二重起動は `serve` が `daemon.lock` で弾く |
| 合成ルート（`src/main.rs`） | launchd plist の install/uninstall、`serve` の single-instance lock 取得、trusted root/environment の受け渡し（#310）。launchd 有無に依存せず起動できる |
| `usagi-daemon` usecase | 起動時 reconcile（未証明の runtime を interrupted へ）、interrupted projection、provider metadata の durable 記録、`ResumeAgent` の scope 再解決と冪等な起動 orchestration |
| `usagi-daemon` infrastructure | `agents.json` への provider metadata の durable atomic write（#311 の writer を再利用）、process identity probe、provider CLI locator |
| provider CLI（Claude/Codex） | 自身の session transcript と resume 意味論の所有者。usagi は provider-native session id/name を渡して `--resume` するだけで、transcript を読まない・保存しない |
| clients（CLI / TUI / MCP） | interrupted の表示と、明示 Resume 操作のトリガーのみ。lifecycle recovery を暗黙に発火しない |

daemon の control authority と durable state は daemon に残す。launchd は「daemon を生かし続ける」だけで、session/agent の権威にはならない。

## 起動時 reconcile と interrupted 可視化

- daemon 起動時、durable operation journal の reconcile（既存契約）に加え、`agents.json` の各 Agent runtime record について process identity を再検証する。crash/reboot 後に identity を証明できない runtime は #209 の `identity_unknown` / `orphan_running` として扱い、**replacement spawn / kill / input を自動で行わない**。
- 当該 Agent が属する `available` session は sidebar から消さず、Agent runtime liveness = **interrupted** として projection する。interrupted は lifecycle 状態ではなく Agent phase / runtime 軸の派生値であり、`available` の projection に read-join する。
- macOS 再起動で子 process も全滅している場合は、生存判定に失敗した runtime を interrupted として記録する。PID の生存だけでは ownership を証明しない（#209）。
- interrupted への遷移は durable state を書き換える必要がある範囲でのみ #311 の atomic writer で永続化し、失敗時は既存 snapshot を置換しない。

## provider resume metadata（`agents.json`）

明示 Resume に必要な最小限だけを durable に保存する。既存の public launch plan snapshot / process identity / runtime state に加える:

| 項目 | 保存 | 備考 |
|---|---|---|
| provider 種別（`claude` / `codex`） | する | adapter registry の code-defined 値 |
| managed-session / worktree identity | する | 既存 scope（SessionId / WorktreeId、trusted root は sessions.json） |
| provider-native session id / name | する | `claude --resume` / `codex resume` に渡す安定識別子 |
| last-known status / phase | する | interrupted 表示と resume 可否判定に使う |
| adapter revision / plan provenance | する | #254 と同じ互換照合に使う |
| argv / environment 値 / secret / token | **しない** | #253/#254 の redaction 契約を維持 |
| provider transcript 本文 | **しない** | provider CLI の所有物。usagi は id/name のみ扱う |

## 明示 Resume 契約

- **入口**: daemon IPC `SessionAction::ResumeAgent`（operation ID 付き）。CLI `usagi session resume-agent <session>`、TUI の Resume 操作、MCP `agent_resume` tool（公開する場合）は全てこの action を呼ぶだけで、client が local に provider CLI を起動しない。
- **明示性**: 既定で **利用者の明示操作からのみ**発火する。TUI 起動・sidebar 再接続・daemon restart・launchd 再起動・通常の session tool はこれを暗黙に呼ばない。
- **動作**: 解決済み managed-session worktree を cwd として、保存済み provider-native session id/name で `claude --resume …` / `codex resume …` 相当の**新規 Agent runtime**を #254 の launch 経路で一度だけ spawn する。これは crash 前の PTY の再 attach ではなく、provider の resume 意味論による新しい会話継続である。
- **fail-closed**: provider CLI 不在は safe `unavailable`、metadata 欠落・adapter 非互換・scope 不一致・worktree 欠損/変更は safe `invalid_argument` / typed rejection とし、spawn しない。ambiguous な identity は #254 どおり fail-closed で人の明示 action を要求する。
- **冪等 / 排他**: 同一 session への concurrent Resume は operation ID / session incarnation で fence し、二重 spawn しない。既存の生存 Agent がある session への Resume は拒否する。

## 失敗モード matrix

| 失敗点 | 期待動作 |
|---|---|
| daemon のみ restart（Agent 生存） | #209 rollover。live terminal は継続、planned restart 後に正しい generation へ再 attach。interrupted にしない |
| Agent process が exit（daemon 生存） | 通常の completion/exit として記録。session は available のまま、Agent は exited。metadata から明示 Resume 可能 |
| daemon crash（Agent は生存し得る） | 起動時 reconcile。identity を証明できない Agent は `identity_unknown`/interrupted。auto replacement/kill しない。明示 Resume で新規継続 |
| SIGKILL / 電源断 | atomic writer 済み snapshot を正本に復元。書込み中断は既存 snapshot を保持し partial を公開しない。runtime は interrupted |
| macOS 再起動 / login | launchd `RunAtLoad` が `serve` を起動。全 Agent は消失しているので interrupted。session は sidebar に残り、明示 Resume 待ち |
| provider transcript / binary が無い | Resume は fail-closed（`unavailable` / `invalid_argument`）。session は interrupted のまま。誤って空継続しない |
| worktree が変更 / 欠損 | scope 再解決で不一致を検出し Resume 拒否。worktree effect を実行しない |
| concurrent Resume | operation ID / incarnation で fence、二重 spawn なし |
| launchd 二重起動 / 手動 `daemon start` と競合 | `serve` の `daemon.lock` が単一インスタンスを保証。lock を取れない process は ready hook に到達せず pid record を消して終了（既存契約） |

## privacy / security

- provider-native session id / name は作業内容を辿れる sensitive な識別子として扱う。durable state・IPC payload・observability log には **id/name と非 secret status のみ**を載せ、transcript 本文・argv・環境変数値・token・raw CLI 出力は載せない（#253/#254 の redaction を維持）。
- 共有・追跡対象になり得る場所（shared log 等）へ provider session id を出す場合は redaction / ハッシュ化を検討し、少なくとも secret と混在させない。
- Resume は必ず daemon が scope 照合した trusted worktree で起動し、IPC client が任意 path/argv/env を指定できない（#255/#264 の terminal launch 契約と同じ境界）。
- launchd plist には絶対 path の daemon binary と最小限の環境のみを書き、secret を含めない。install は明示 opt-in。

## observability / logging

- 既存の `<data-dir>/logs/error-YYYY-MM-DD.log` に、起動時 reconcile の結果（interrupted 化した runtime 数、理由の分類）、Resume の試行と結果（成功 / typed failure）を記録する。id はログ方針に従い redaction する。
- launchd 供給時に detached serve の stderr が破棄されても、起動失敗・異常終了・reconcile 結果を日次 error log から追跡できることを保証する（既存 failure logging 契約の延長）。
- TUI の観測経路（metrics observer）は本 issue で新規計装を必須にしないが、interrupted 件数を将来 surface できる形にしておく。

## migration / backward compatibility

- provider metadata を持たない既存 `agents.json` record は「provider resume 不可」として扱い、interrupted 表示はするが Resume は fail-closed にする（後方互換・no crash）。metadata は次回以降の launch から充足する。
- launchd 供給は **opt-in**。install しない環境・非 macOS では従来の `usagi daemon start`（detached）と debug の `develop/` 分離（`cargo run`）を壊さない。
- `agents.json` の schema 追加は durable atomic writer（#311）で前方後方互換に読み書きし、未知/欠損フィールドで起動を失敗させない。

## rollout（段階）

1. interrupted 可視化 + provider metadata 永続化（reconcile と projection、client は表示のみ）。
2. 明示 Resume 経路（IPC `ResumeAgent` + CLI、後に TUI / MCP）。
3. launchd LaunchAgent の install/uninstall（`usagi daemon install-service` 等）と `RunAtLoad`/`KeepAlive` 供給。既定は opt-in。
4. （別 issue）auto-resume の opt-in 検討。

## 受け入れ条件

- launchd LaunchAgent を install すると、macOS login / 再起動 / daemon の異常終了後に `usagi daemon serve` が単一インスタンスとして再起動し、`daemon.lock` の権威と二重起動防止が保たれる。uninstall で supervision が止まる。
- daemon-only restart（Agent 生存）は #209 rollover のままで、session を interrupted にせず正しい generation へ再 attach できる。
- daemon crash / SIGKILL / macOS 再起動後の起動で、identity を証明できない Agent runtime は interrupted として reconcile され、replacement spawn / kill / input を自動で行わない。当該 `available` session は sidebar に残り、legacy UI metadata（#348/#349 経由）を失わない。
- `agents.json` に provider 種別・worktree identity・provider-native session id/name・last-known status・adapter revision が durable に保存され、secret / argv / transcript は保存されない。
- 明示 `SessionAction::ResumeAgent`（CLI / TUI / MCP から）だけが provider resume を起動し、通常の起動・再接続・restart・launchd 再起動では発火しない。auto-continue は存在しない。
- Resume は解決済み trusted worktree で新規 Agent を一度だけ spawn し、provider CLI 不在・metadata 欠落・adapter 非互換・scope/worktree 不一致・concurrent 要求の各ケースで fail-closed（二重 spawn / worktree effect / secret 露出なし）。
- failure matrix の全ケースで、既存 durable state は byte-equivalent に保たれ partial snapshot を公開しない。

## テスト方針

- fake launchd 相当（composition root で差し替え可能な service installer 境界）で plist install/uninstall と `RunAtLoad`/`KeepAlive` 供給を検証する。実 launchd 登録なしで install 生成物と単一インスタンス lock の相互作用を確認する。
- daemon restart / 擬似 crash（process 消失）/ macOS reboot 相当（全 runtime identity 失効）の runtime integration test で、interrupted reconcile と no-auto-replacement を検証する。
- `agents.json` の provider metadata の round-trip、redaction（secret/argv/transcript 非保存）、後方互換（metadata 欠落 record）の store test。
- `ResumeAgent` の scope 再解決・冪等 fence・fail-closed 各分岐を fake provider CLI fixture で検証する（実 CLI / 実 login 不要、#254 fixture 経路を再利用）。
- TUI `FsWorkspaceLoader` / projection の regression で、restart 後に interrupted session が sidebar に stable ID で残り metadata を保つことを検証する。

## ドキュメント

- `document/05-daemon.md` を正本として、launchd supervision（serve と lock の役割分担）、起動時 reconcile と interrupted 可視化、`agents.json` の provider resume metadata と redaction、明示 Resume 契約（auto-continue 非対応）、failure matrix、所有境界を追記する。
- `document/02-architecture.md` は launchd installer（合成ルート所有）と provider resume metadata store への参照だけを持つ。
- launchd / crash 継続の将来設計（#221 broker）との境界を明記し、本 issue が explicit orphan 契約を維持することを記載する。
- auto-resume を非対応とする方針を明記する（実装済み仕様のみ記載の規約に従い、opt-in 予定は issue store に残す）。
