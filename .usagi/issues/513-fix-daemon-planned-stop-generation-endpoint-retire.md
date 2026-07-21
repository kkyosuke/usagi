---
number: 513
title: fix(daemon): planned stop で generation endpoint を安全に retire する
status: done
priority: high
labels: [review, v2, daemon, lifecycle, ipc, safety]
dependson: []
related: [216, 341, 171, 209]
created_at: 2026-07-21T21:37:42.453841+00:00
updated_at: 2026-07-21T22:09:43.239305+00:00
---

## 問題・影響

v2 の planned `usagi daemon stop` は記録 PID へ SIGTERM を送り `daemon.json` を消すが、daemon owner が公開した active `current.json` と generation Unix socket node を回収しない。PID 消滅後も `connect_current` は locator を解決して旧 socket へ接続し `ConnectionRefused` になる。`connect_or_start` は #341 の split-brain 防止契約により `NotFound` 以外で replacement daemon を自動起動しないため、CLI / TUI / MCP は利用者が明示的に `daemon start` するまで unavailable になる。明示 start 後も旧 generation socket node は残留する。

## 成立条件 / 実測

最新 `origin/main d74ed7d`（指定基点 `edb20b5f` の直系 1 commit 後。daemon 差分なし）から release binary を build し、隔離 `USAGI_HOME` と `USAGI_RUNTIME_MODE=production` で確認した。

1. `usagi daemon start` で PID と active generation を取得する。
2. `usagi daemon stop` 後、記録 PID の消滅を待つ。
3. `daemon.json` は消えるが `daemon/current.json` と locator が指す `generations/<generation>/sock` は残る。
4. `usagi session remove missing` は exit 1、`daemon endpoint is unavailable` となり autostart しない。
5. 明示 `usagi daemon start` は新 generation を公開するが旧 socket は残る。

原因は、`serve` が shutdown 後に PID record だけを clear し、IPC accept loop が shutdown flag を監視しない detached infinite threadとして `SecureUnixListener` を所有することにある。process exit では worker の Rust destructor が走らず、listener の既存 Drop も socket だけを unlink して locator を retire しない。また shutdown handler / wait の準備が endpoint publish と worker spawn の後なので、process-directed signal が handler 登録前または未準備の worker に配送され default termination すると owner cleanup 自体を迂回しうる。

## 修正方針

- daemon owner の lifecycle を `shutdown signal prepare → endpoint publish / worker spawn → wait → admission/accept loop stop → join → daemon record clear → generation-fenced endpoint retire → lock release` の順にする。
- SIGINT / SIGTERM handler と同期 wait は worker spawn より前に登録する。process の signal mask は変更せず、その後起動する child process へ blocked signal を継承させない。
- accept loop は共有 shutdown を監視して有界に終了し、owner が join して listener ownership を回収する。join 後に daemon record を clear してから `SecureUnixListener` を retire / Drop し、全 cleanup が終わるまで instance lock を手放さない。record が残る短い区間では replacement start が既存 PID を見て抑止されるため、locator を先に消して autostart を readiness timeout へ誘導しない。
- retire は自 generation の socket を回収し、`current.json` が同じ generation と endpoint をまだ指す場合だけ locator を unlink する。stale generation は replacement generation の locator / endpoint を削除してはならない。比較と unlink の競合を serialization し、TOCTOU で新 locator を消さない。
- #341 の「locator 存在時の接続失敗・draining・不正 endpoint では別 daemon を勝手に起動しない」契約は弱めない。正常 planned stop が locator を安全に消すことで、次の client が `NotFound` 経路から一度だけ autostart できるようにする。
- cleanup error は黙って成功扱いせず、record / ownership を安全側に残して診断可能にする。panic や crash の stale endpoint recovery は本 issue の planned stop と混同しない。

## 受入条件

- [x] planned stop 後、記録 PID、active `current.json`、当該 generation socket が消える。
- [x] stop 後の CLI / TUI / MCP bootstrap は `NotFound` を受け、新 daemon を一度だけ起動して利用可能になる。
- [x] `ConnectionRefused` など locator 存在下の failure は従来どおり autostart せず fail closed になる。
- [x] stale generation の遅延 retire / Drop は、既に公開された replacement `current.json` と socket を保持する。
- [x] accept loop は shutdown を観測して終了し、daemon owner が join してから record を clear し、その後 listener の generation-fenced retire / Drop を完了する。
- [x] shutdown signal handler / wait の準備が endpoint publish / worker spawn より前であることを usecase ordering test と実 process testで固定する。
- [x] transport と daemon lifecycle の v2 正本 docs を実装へ整合する。

## 必須回帰テスト

- `tests/cli_tui.rs` の実 Unix / production binary lifecycle testで、start → endpoint ready → stop → PID 消滅 → locator/socket 消去 → client autostart / 新 generation を検証する。
- Unix transport testで old generation 公開 → replacement generation 公開 → old retire の順を作り、replacement locator / socket / connectability が保持されることを検証する。
- serve lifecycle fakeで prepare / publish / wait / stop+join / record clear / retire の順序と cleanup failure を検証する。
- 既存 bootstrap testの non-absence failure で start 回数 0 を維持する。

## docs / gate

`document/04-ipc.md` の Unix transport と `document/05-daemon.md` の process lifecycle / data directory を更新する。Rust / process / signal / thread / Unix IO に影響するため、selected tests、fmt、workspace check / clippy を実行し、full test / coverage 100% は PR CI の必須 gate とする。
