# 12. terminal VT snapshot（raw tail → semantic checkpoint）

> [設計提案一覧](README.md) ｜ [ドキュメント目次](../README.md) ｜ ← 前へ [workspace open 時の pane 復元](11-workspace-restore-panes.md)

本書は [#524](../../.usagi/issues/524-fix-terminal-raw-64kib-tail-vt-parser-safe-snapshot.md)（P1 correctness）の**未実装設計**である。attach / resync snapshot を「blank parser に任意の raw 64 KiB tail」から **versioned semantic screen checkpoint** へ置き換え、[#199](../../.usagi/issues/199-perf-daemon-vt100-scrollback-daemon.md) が定めた「daemon を terminal grid/scrollback の唯一の権威とする」契約を回復するための設計判断を確定する。本書が採用機構・wire schema・互換 matrix・allocation 上限の設計判断の正本であり、実装が確定したら該当部分を [4. daemon IPC](../04-ipc.md)（snapshot schema / capability / revision / geometry / offset）と [3. TUI](../03-tui.md)（visible + primary/copy-history restore）へ畳み込む。

## 目次

- [前提と問題](#前提と問題)
- [非目標](#非目標)
- [中心設計判断](#中心設計判断)
- [層と serialization 責務](#層と-serialization-責務)
- [checkpoint schema](#checkpoint-schema)
- [allocation 上限と hostile decode](#allocation-上限と-hostile-decode)
- [capability / revision negotiation と互換 matrix](#capability--revision-negotiation-と互換-matrix)
- [geometry / revision fence](#geometry--revision-fence)
- [offset continuity と resume suffix](#offset-continuity-と-resume-suffix)
- [受入条件 → 実現方法](#受入条件--実現方法)
- [必須回帰テスト → 実装場所](#必須回帰テスト--実装場所)
- [実装 issue 分割](#実装-issue-分割)
- [docs 畳み込み先](#docs-畳み込み先)

## 前提と問題

現行 shipping v2 の terminal replay 経路は次のとおりである。

| 層 | 現状 | 置き場所 |
|---|---|---|
| daemon | `TerminalRegistry` が **raw PTY byte** を最大 64 KiB の bounded journal に保持し、attach/resync に `Snapshot { base_offset, output_offset, geometry, replay: Vec<u8>, exited }` を返す。`replay` は raw byte tail である | `crates/daemon/src/usecase/terminal.rs` |
| wire | daemon `Snapshot` を JSON serialize し、TUI が `TerminalAttach { subscription, connection_epoch, output_offset, replay: Vec<u8>, exited }` へ decode する | `crates/core/src/usecase/client.rs` / `crates/tui/src/presentation/` |
| TUI | `TerminalSession::replace` が **blank `TerminalScreen` を作り** `screen.advance(&attach.replay)` で raw tail を先頭から VT parser へ流す | `crates/tui/src/usecase/application/terminal_session.rs` |
| VT parser | `TerminalScreen`（grid / scrollback / cursor / SGR / scroll region / alternate buffer / saved cursor / UTF-8 decoder）は **TUI クレートにのみ存在** | `crates/tui/src/usecase/application/terminal_screen.rs` |

**問題**: 64 KiB tail は任意 byte 境界で切られるため UTF-8 / CSI / OSC sequence の途中から始まり得る。さらに tail 以前に確立された cursor 位置、SGR、scroll region、alternate/saved buffer、消去・折返し状態を一切含まない。blank parser に流しても現在の screen state を再構成できず、trim 後の attach/reconnect で文字化け・escape 漏れ・cursor/画面/copy history 破損を起こす。これは daemon を grid authority とした #199 の shipping regression である。

trim で失われた pre-window state を復元する方法は「**daemon が VT parser を実行して semantic な screen state を保持し、それを snapshot として送る**」以外に存在しない。raw byte only の設計を保ったまま pre-window の cursor/SGR/alt を再構成することは原理的に不可能である（情報が journal trim で消えている）。したがって daemon が parser を持つことは correctness 上の必須要件であり、本設計の起点とする。

## 非目標

- [#472](../../.usagi/issues/472-fix-daemon-terminal-output-pipeline-bounded-frame-safe.md) が所有する end-to-end の byte/frame bound と bounded journal 自体は再実装しない。本設計は bounded window を**維持したまま**、その window を raw bytes から semantic checkpoint へ置き換える。
- [#473](../../.usagi/issues/473-fix-daemon-exited-pty-map-entry-fd.md) の exited PTY transport / FD 回収は変更しない。
- snapshot restore を理由に PTY を respawn しない。child PID / spawn count は reattach 前後で不変を保つ。
- daemon crash 後の terminal 継続（broker / FD handoff）は [7. PTY crash 継続](07-pty-crash-continuation.md) の範囲であり、本設計はその explicit orphan 契約を変えない。

## 中心設計判断

3 つの判断で構成する。

1. **VT parser authority を `usagi-core` へ移す**。daemon と TUI で parser を二重実装しないため、`TerminalScreen` の **VT state model + parser + serialize/deserialize** を `usagi-core`（pure・IO なし・`serde` derive 可）へ移す。TUI 側の**描画**（`rows_with_scrollback_and_cursor_selection` / link scan / selection / cursor marker）は presentation 語彙に依存するため TUI に残し、core が公開する read-only な cell API（`ch` / interned style / continuation / cursor / scrollback）の上に載せ替える。
2. **daemon を grid authority にする**。daemon は terminal ごとに core の VT screen を 1 個保持し、`append_output` で受信 byte を feed、`resize` で screen を resize する。attach/resync snapshot は raw tail ではなく core screen の **semantic checkpoint** を返す。#199 の「daemon が viewport snapshot・cursor・attrs を送る」契約を回復する。
3. **checkpoint + contiguous suffix**。attach/resync は `output_offset` 時点の完全な semantic checkpoint を返す（`replay` raw tail は撤去）。増分は従来どおり `Resume { after_offset }` が `output_offset` 以降の **contiguous raw byte suffix** を返し、TUI は checkpoint から復元した parser にその suffix を feed する。checkpoint が「complete な再構築表現」、suffix が「checkpoint 以後の連続増分」を担う。

```text
                 raw PTY bytes
                      │
                      ▼
         daemon: core::VtScreen (authority)  ── #199 grid/scrollback owner
          │ append_output → feed              │
          │ resize        → resize            │
          ▼                                   ▼
   attach/resync:                       Resume{after_offset}:
   ScreenCheckpoint @ output_offset     raw suffix (output_offset..next)
          │                                   │
          ▼                                   ▼
         TUI: core::VtScreen::from_checkpoint(...) → advance(suffix)
                      │
                      ▼
         TUI 描画（selection / link / cursor marker）
```

この分割により、raw PTY bytes（transport／resume 増分）と rendered screen（checkpoint）を混同しない。

## 層と serialization 責務

| 責務 | 置き場所 | 根拠 |
|---|---|---|
| VT parse・grid/scrollback/alt/saved/decoder state・resize | `usagi-core`（usecase 層の pure 型 `VtScreen`） | 単一 parser authority。daemon/TUI が共有。core domain には置かず usecase 層に置く（`unicode-width` 依存を domain に持ち込まない） |
| checkpoint 型・serde・bounded decode | `usagi-core`（`VtScreen::checkpoint()` / `VtScreen::from_checkpoint()`） | serialize 責務を authority と同居させ、daemon/TUI で別実装しない |
| PTY 所有・byte 受信・checkpoint 生成・frame bound 計上 | `usagi-daemon`（`TerminalRegistry`） | grid authority。IPC frame と memory bound を daemon で強制 |
| checkpoint → screen 復元・描画・selection/copy | `usagi-tui` | presentation 語彙は core へ逆流させない |

`unicode-width` は現在 `usagi-tui` のみが使う。parser を core へ移すのに伴い `usagi-core` の依存に追加する（[06-conventions.md#依存クレート](../06-conventions.md#依存クレート) の「必要になった時点で追加」に従い、追加時に同表を更新する）。core domain の依存規則（`chrono`/`serde`/`uuid` のみ）は変えず、`unicode-width` は usecase 層でのみ使う。

## checkpoint schema

wire は既存の generation 1 に revision 2 を追加して運ぶ。`Snapshot.replay: Vec<u8>` を廃し、revision 2 では `Snapshot.screen: ScreenCheckpoint` を持つ。`base_offset` は checkpoint が `output_offset` 時点の完全 state を表すため常に `base_offset == output_offset`（tail 長 0）となる。

```rust
// usagi-core（usecase 層 `vt_screen::checkpoint`）。すべて serde derive。
pub struct ScreenCheckpoint {
    pub schema_version: u16,          // checkpoint schema。SCHEMA_VERSION（初版 1）
    pub geometry: Geometry,           // { rows: u32, cols: u32 }
    pub active: ActiveBuffer,         // Primary | Alternate
    pub primary: BufferCheckpoint,    // 常に存在（cells_with_scrollback / copy history の権威）
    pub alternate: Option<BufferCheckpoint>, // alternate が active のときだけ Some
    pub styles: Vec<String>,          // interned SGR 文字列表（attribute table）。cell は index 参照
    pub decoder: DecoderCheckpoint,   // parser/decoder の途中状態
}

pub struct BufferCheckpoint {
    pub grid: Vec<RowCheckpoint>,        // 可視 grid（rows 行）
    pub scrollback: Vec<RowCheckpoint>,  // 履歴（≤ SCROLLBACK_MAX）
    pub cursor: (u32, u32),              // row, col（col は wrap-pending の 1 桁分だけ cols を許容）
    pub saved_cursor: Option<(u32, u32)>,// DECSC / SCP
    pub scroll_region: (u32, u32),       // DECSTBM top, bottom
    pub style_id: u32,                   // 次に印字するセルの SGR（styles への index）
}

pub struct RowCheckpoint {
    // run-length。(style_id, ch, continuation, repeat) の連。空白 padding を圧縮
    pub runs: Vec<CellRun>,
}
pub struct CellRun { pub style_id: u32, pub ch: char, pub continuation: bool, pub repeat: u32 }

pub struct DecoderCheckpoint {
    pub phase: DecoderPhase,   // Ground | Escape | Csi | Osc | Charset
    pub params: String,        // CSI 収集途中（≤ PARAMS_MAX）
    pub utf8_pending: Vec<u8>, // ≤ UTF8_PENDING_MAX（3 byte）
    pub utf8_needed: u8,       // ≤ UTF8_NEEDED_MAX（4）
}
```

`VtScreen::checkpoint()` が上記を生成し、`VtScreen::from_checkpoint(&ScreenCheckpoint)
-> Result<VtScreen, CheckpointError>` が bounded に復元する。serialized wire の運搬は
`ScreenCheckpoint::to_json_bytes()` / `from_json_bytes()` が担い、`CHECKPOINT_BYTES_MAX`
を確保前に強制する（parse 前に過大 payload を弾く）。

**設計上の要点**

- **primary saved buffer を明示保存**。現行 `VtScreen` は alternate 中に primary を `primary_screen` へ退避する。checkpoint では常に `primary`（背景の primary）を保存し、alternate が active なら `alternate` に可視 alt を保存する。これで alternate から戻った後の primary buffer・`cells_with_scrollback`・selection/copy history が untrimmed reference と一致する（受入条件 2/3）。
- **interned style table**。cell は SGR 文字列を直接持たず `styles` の index を参照する（各 buffer の「次に印字する SGR」も `style_id` で index 参照）。反復する style を 1 度だけ運び、hostile な「巨大 attribute table」は table 長の上限で decode 前に拒否できる。
- **decoder state を含む**。checkpoint が UTF-8 / CSI / OSC の途中で取られても、`decoder` が phase・params・utf8_pending を保持するため、以後の suffix が sequence を正しく継続する（受入条件 1）。
- **run-length**。agent が描く空白 padding 中心の画面を圧縮し、frame/memory bound 内に収める。

## allocation 上限と hostile decode

decode は **算術検証 → 予算検証 → 確保** の順に行い、途中の escape を文字として露出しない。上限（初期値。実装で調整）:

| 上限 | 値（初期） | 目的 |
|---|---|---|
| `ROWS_MAX` / `COLS_MAX` | 1024 / 2048 | geometry の上限。乗算前に個別検証 |
| `CELLS_PER_TERMINAL_MAX` | `checked_mul(rows, cols)` + scrollback を含む cell 総数の上限 | overflow・巨大 grid を拒否 |
| `SCROLLBACK_MAX` | 10 000 行 | 現行 TUI と同一。checkpoint に載る scrollback を bound |
| `STYLES_MAX` | 4 096 | interned attribute table 長 |
| `PARAMS_MAX` / `UTF8_PENDING_MAX` | 64 byte / 3 byte | decoder 途中状態の bound |
| `CHECKPOINT_BYTES_MAX` | 既定 1 MiB frame − envelope 余裕 | 単一 checkpoint の serialized 上限 |
| aggregate cell budget | process-local 合算上限 | 全 terminal 合計の cell/scrollback memory peak を bound |

- すべての count・index・`rows*cols` は `checked_*` 算術で検証し、`style_id` は `styles.len()` 未満を検証する。範囲外・overflow・未知 `schema_version` は **typed fail closed**（`ProtocolError` / `RegistryError::ResyncRequired` 等）で拒否し、panic・unbounded allocation・blank parser corruption を起こさない。
- daemon は checkpoint 生成時に `CHECKPOINT_BYTES_MAX` と aggregate budget を強制する。scrollback がこれを超える場合は **古い行から bounded に trim**（copy history 契約の範囲で）してから載せ、frame を超えない。counters（`terminal_dropped_bytes` 等）に相当する trim 計上を追加する。
- compression bomb 相当（小さな payload が巨大 allocation を誘発）に対しては、`repeat` と行数・table 長を確保前に合算検証する。

## capability / revision negotiation と互換 matrix

- daemon は `ServerHello.capabilities` に `terminal.screen-checkpoint.v1` を広告する。
- wire は generation 1 の `max_revision` を **1 → 2** に上げる。revision 2 が semantic checkpoint、revision 1 が legacy raw tail。negotiate は既存どおり共通 range の最大 revision を選ぶ。
- 新 client は capability があり revision 2 が共通なら checkpoint 経路を使う。共通 revision が 1 に落ちる（＝旧 daemon）場合は **legacy raw tail を parser へ流さない**。安全な限定表示（"履歴を復元できません" 相当の typed state）を出し、`output_offset` からの live 出力だけを描画する（途中 escape を文字として露出しない）。

| client | daemon | capability | 共通 revision | 収束先 |
|---|---|---|---|---|
| new | new | present | 2 | negotiated semantic checkpoint（目標経路） |
| new | old | absent | 1 | client が legacy 限定表示へ fail closed（raw tail を parse しない） |
| old | new | present | 1 | daemon が revision 1 の raw tail を返す（旧 client の既存挙動を維持） |
| new | new | present | 未知 revision のみ | 共通 range 無し → typed incompatible（handshake 拒否） |
| new | new | absent（advertise 漏れ） | 2 が共通でも capability 不在 | client は checkpoint を要求せず legacy 限定表示（capability を真実源とする） |

移行期間中 daemon は revision 1（raw）と revision 2（checkpoint）の**両方**を negotiated revision に応じて提供する。次の incompatible wire generation で revision 1 と `replay` field を撤去する。

## geometry / revision fence

- checkpoint は `geometry` と daemon `Snapshot.revision` を含む。TUI は復元後、`resize` を送る前後で `revision` / `geometry` の不一致を検出したら old/new state を混在させず、**snapshot retry または typed resync** に落とす。
- resize が「checkpoint 直前 / 生成と suffix の間 / restore 直後」に割り込んでも、daemon の terminal actor 排他区間（[4. daemon IPC#generic terminal request](../04-ipc.md#generic-terminal-request) の resize preflight→effect→commit）と revision fence により、TUI は geometry mismatch を検出して retry する。checkpoint の cells は新 geometry で control byte を再生せず、既存 cell を clip/pad する（現行 `TerminalScreen::resize` と同一方針）。

## offset continuity と resume suffix

- `output_offset` の意味は不変（daemon が受理した累積 byte offset）。checkpoint は「`output_offset` までを反映した state」を表す。
- `Resume { after_offset }` の契約（[4. daemon IPC#generic terminal request](../04-ipc.md#generic-terminal-request)）は不変: window 内なら `after_offset` から始まる contiguous raw suffix を返し、window より古ければ `resync_required`。TUI は resync 時に checkpoint で screen を置換して `output_offset` から resume を続ける。
- suffix は依然 raw bytes だが、**checkpoint で復元済みの parser** に feed するため、blank parser 問題は起きない。suffix 先頭は必ず既知の parser phase（checkpoint の `decoder`）に接続する。

## 受入条件 → 実現方法

| # | 受入条件 | 実現方法 |
|---|---|---|
| 1 | retention 先頭が UTF-8/CSI/OSC/SGR/alt の途中でも reconnect 前後の visible cells/cursor/style が一致 | daemon parser が semantic checkpoint を生成。tail 先頭の byte 境界に依存しない |
| 2 | primary/alternate/saved primary buffer・`cells_with_scrollback`・selection/copy history が untrimmed reference と一致 | `primary` を常時保存 + `alternate` を条件保存。scrollback を checkpoint に含める |
| 3 | tail 以前の cursor 移動・clear・scroll region・alternate の状態が保持 | daemon が全 byte を parse 済みで、checkpoint に cursor/scroll_region/alt/saved を含む |
| 4 | resize が checkpoint 前後に interleave しても old/new state を混在させない | geometry/revision fence + terminal actor 排他 |
| 5 | malformed/unknown revision・hostile dimensions/counts が typed fail closed | 算術検証・予算検証・後に確保。escape injection/panic/overflow/unbounded allocation を防止 |
| 6 | checkpoint+suffix が既定 frame と per-terminal/aggregate memory bound 内 | run-length + interned style + `CHECKPOINT_BYTES_MAX` + aggregate budget + bounded scrollback trim |
| 7 | old/new client-daemon の全組合せが決定的収束 | [互換 matrix](#capability--revision-negotiation-と互換-matrix) |
| 8 | Agent/generic・resize・resync・exit final で同一 contract、reattach 前後で PID/spawn count 不変 | attach 系 stream 語彙は Agent/generic 共通（[4. daemon IPC](../04-ipc.md#agent-launch-request)）。checkpoint は既存 lifecycle に載せ respawn しない |

## 必須回帰テスト → 実装場所

| # | テスト | 実装場所（予定） |
|---|---|---|
| 1 | 実 daemon + 実 PTY + fresh client の E2E（64 KiB 超・SGR・alt・save/restore・copy marker）で reattach 後の PID/spawn count 不変・visible/cursor/style・saved primary・`cells_with_scrollback`/copy history 一致 | `crates/daemon/tests/`（`agent_real_pty.rs` 隣接の新 test target） |
| 2 | 64 KiB 超出力で UTF-8/CSI/OSC/SGR/alt/combining/CJK/malformed の**全 split 位置**を、untrimmed reference parser と checkpoint+suffix restore で property/fixture 比較 | `usagi-core` の parser unit + property test |
| 3 | old/new client × old/new daemon × capability present/absent × supported/unknown revision の compatibility matrix を固定し、途中 escape を legacy raw parser へ渡さないことを assert | `usagi-core` negotiation test + `usagi-tui` fail-closed test |
| 4 | rows/cols 0・最大値・乗算 overflow・巨大 cell/attribute/scrollback count・aggregate 超過・compression bomb 相当を fuzz/property し、確保前 bounded rejection を測定 | `usagi-core` の `from_checkpoint` decode test |
| 5 | resize を checkpoint 直前 / capture 中 / suffix 適用前後へ barrier で interleave し、geometry/revision mismatch が retry/typed resync になり state 混在しないことを検証 | `usagi-daemon` terminal registry test + `usagi-tui` session test |
| 6 | 実 IPC frame size・per-terminal/aggregate allocation peak・Agent/generic 共通 fixture・exit final snapshot を assert | `usagi-daemon` test（`write_json_frame` で frame bound を assert する既存パターンを流用） |

## 実装 issue 分割

100% coverage gate 下で reviewable に保つため、次の順で PR を分割する。各 PR は緑になってから次へ進む。実装 issue は #524 を親に持ち、`dependson` で線形に連結する（#532 → #533 → #534 → #535 → #536）。

| Phase | issue | 内容 | テスト |
|---|---|---|---|
| 1 | [#532](../../.usagi/issues/532-refactor-core-vt-parser-usagi-core.md) | **core: VT parser 抽出**。`TerminalScreen` の state+parser+resize を `usagi-core` の `VtScreen` へ移し、TUI は core screen を wrap して描画のみ担当。挙動不変の pure refactor | 既存 test を core へ移送 |
| 2 | [#533](../../.usagi/issues/533-feat-core-screencheckpoint-bounded-hostile-decode.md) | **core: checkpoint 型と bounded decode**。`ScreenCheckpoint` / `from_checkpoint` / `checkpoint` と全上限・checked 算術・typed rejection | #2 / #4 |
| 3 | [#534](../../.usagi/issues/534-feat-daemon-terminal-grid-authority-revision-2-checkpoint-snapshot.md) | **daemon: grid authority**。`TerminalRegistry` に per-terminal `VtScreen` を持たせ append_output/resize で feed。revision 2 の `Snapshot.screen` 生成、frame/aggregate bound 強制、`terminal.screen-checkpoint.v1` 広告 | #5 / #6 |
| 4 | [#535](../../.usagi/issues/535-fix-tui-checkpoint-negotiation-screen-reconstruct-legacy-fail-closed.md) | **wire + tui: negotiation と reconstruct**。revision 2 negotiation、`TerminalAttach` を checkpoint 版へ、`TerminalSession::replace` を `from_checkpoint`+suffix へ、legacy fail-closed。**この Phase で P1 correctness が解消** | #3 |
| 5 | [#536](../../.usagi/issues/536-test-daemon-pty-reattach-checkpoint-e2e-proposal.md) | **E2E + docs 畳み込み**。実 daemon + 実 PTY の reattach 一致。[04-ipc.md](../04-ipc.md) / [03-tui.md](../03-tui.md) を更新し、本提案を「畳み込み済み」に落とす | #1 |

root は committed の #524 が todo かつ生存 session が無い状態を ready 候補として扱えるため、実装は Phase 1（#532）から `dependson` の順に session 委譲する。#524 自体は最終 Phase（#536）完了時に `done` にする。

## docs 畳み込み先

実装確定後、次の正本へ畳み込み、本書は README 一覧でリンクだけ残す。

- [4. daemon IPC](../04-ipc.md#generic-terminal-request): snapshot schema（`replay` → `screen` checkpoint）、`terminal.screen-checkpoint.v1` capability、generation 1 revision 2、geometry/offset 契約、互換 matrix、hostile allocation 上限。
- [3. TUI](../03-tui.md#live-terminal-の出力表示と入力): visible + primary/copy-history restore behavior、legacy fail-closed 限定表示。
- [06-conventions.md#依存クレート](../06-conventions.md#依存クレート): `unicode-width` を `usagi-core`（usecase 層）でも使う旨。
