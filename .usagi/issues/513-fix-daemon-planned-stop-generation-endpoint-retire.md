---
number: 513
title: fix(daemon): planned stop で generation endpoint を安全に retire する
status: in-progress
priority: high
labels: [review, v2, daemon, lifecycle, ipc, safety]
dependson: []
related: [216, 341, 171, 209, 507, 515]
created_at: 2026-07-21T21:37:42.453841+00:00
updated_at: 2026-07-21T23:41:00+00:00
---

## 問題・影響

v2 の planned `usagi daemon stop` は記録 PID へ SIGTERM を送り `daemon.json` を消すが、daemon owner が公開した active `current.json` と generation Unix socket node を回収しない。PID 消滅後も `connect_current` は locator を解決して旧 socket へ接続し `ConnectionRefused` になる。`connect_or_start` は #341 の split-brain 防止契約により `NotFound` 以外で replacement daemon を自動起動しないため、CLI / TUI / MCP は利用者が明示的に `daemon start` するまで unavailable になる。明示 start 後も旧 generation socket node は残留する。

## 成立条件 / 実測

最新 `origin/main e047610b`（指定基点 `edb20b5f` の直系。後続変更を含め planned stop の競合条件は同じ）から release binary を build し、隔離 `USAGI_HOME` と `USAGI_RUNTIME_MODE=production` で確認した。

1. `usagi daemon start` で PID と active generation を取得する。
2. `usagi daemon stop` 後、記録 PID の消滅を待つ。
3. `daemon.json` は消えるが `daemon/current.json` と locator が指す `generations/<generation>/sock` は残る。
4. `usagi session remove missing` は exit 1、`daemon endpoint is unavailable` となり autostart しない。
5. 明示 `usagi daemon start` は新 generation を公開するが旧 socket は残る。

原因は、`serve` が shutdown 後に PID record だけを clear し、IPC accept loop が shutdown flag を監視しない detached infinite threadとして `SecureUnixListener` を所有することにある。process exit では worker の Rust destructor が走らず、listener の既存 Drop も socket だけを unlink して locator を retire しない。また shutdown handler / wait の準備が endpoint publish と worker spawn の後なので、process-directed signal が handler 登録前または未準備の worker に配送され default termination すると owner cleanup 自体を迂回しうる。

追加レビューでは record cleanup 自体にも replacement 消失 race を確認した。stop が record A を load して SIGTERM を送った後に停止し、owner cleanup が A を消去して lock を解放、client が replacement B を起動して B を save した後、古い stop が再開して無条件 remove すると B の `daemon.json` まで消える。stale reclaim と owner の遅延 cleanup にも同じ load-then-remove gap があり、pid だけの比較では pid 再利用時の incarnation を区別できない。

## 修正方針

- daemon owner の lifecycle を `shutdown signal prepare → endpoint publish / worker spawn → wait → admission/accept loop stop → join → generation-fenced endpoint retire → exact daemon record clear → lock release` の順にする。
- SIGINT / SIGTERM handler と同期 wait は worker spawn より前に登録する。handler は signal 受理時点で shared shutdown fence を立て、重い publish 初期化中に signal が届いても後続 accept loop が request を受理しないようにする。process の signal mask は変更せず、その後起動する child process へ blocked signal を継承させない。
- accept loop は共有 shutdown を監視して有界に終了し、owner が join して listener ownership を回収する。`SecureUnixListener` の retire 成功後だけ exact daemon record を clear し、全 cleanup が終わるまで instance lock を手放さない。record を completion fence として最後まで残すため、retire failure は replacement を起動せず診断可能なまま fail closed になる。locator retire と record clear の短い間に client が `NotFound` を見ても、live record と instance lock が replacement を抑止する。
- retire は自 generation の socket を回収し、`current.json` が同じ generation と endpoint をまだ指す場合だけ locator を unlink する。stale generation は replacement generation の locator / endpoint を削除してはならない。比較と unlink の競合を serialization し、TOCTOU で新 locator を消さない。
- #341 の「locator 存在時の接続失敗・draining・不正 endpoint では別 daemon を勝手に起動しない」契約は弱めない。正常 planned stop が locator を安全に消すことで、次の client が `NotFound` 経路から一度だけ autostart できるようにする。
- cleanup error は黙って成功扱いせず、record / ownership を安全側に残して診断可能にする。panic や crash の stale endpoint recovery は本 issue の planned stop と混同しない。
- running stop は SIGTERM 後に record を先行 clear せず、owner が endpoint retire 後に exact record を変更・消去するまで bounded poll する。owner が record を残したまま消滅または timeout した場合は cleanup failure として record を保持する。最初から stale だった record と serve cleanup は保持した完全な `(pid, started_at)` だけを conditional clear する。実 filesystem adapter は `daemon.json` の read/save/conditional clear を stable な `record.lock` の同一 cross-process transaction で直列化し、比較と unlink を分離しない。raw PID の process identity 問題は #514 の範囲とし、本 issue へ混ぜない。
- record save は live `daemon.json` を truncate せず、private unique temporary の write / fsync / owner 検証後に同一 directory で atomic rename し、parent directory を best-effort fsync する。write / rename error を返す経路では旧 record を保持して temporary unlink を試み、その cleanup failure も error として返す。hard crash では unique temporary が残り得るが、後続 save は別名を使う。conditional unlink も parent fsync を試行し、rename / unlink 後の非対応 directory fsync は commit 済み操作を曖昧な error にしない。
- endpoint publish が ordinary error を返す場合は、owner object 構築前でも自 generation socket と作成済み temporary の rollback を試み、rollback failure も error として返す。process crash 後の stale endpoint / temporary recovery は #515 の範囲とする。

## 受入条件

- [x] planned stop 後、記録 PID、active `current.json`、当該 generation socket が消える。
- [x] stop 後の CLI / TUI / MCP bootstrap は `NotFound` を受け、新 daemon を一度だけ起動して利用可能になる。
- [x] `ConnectionRefused` など locator 存在下の failure は従来どおり autostart せず fail closed になる。
- [x] stale generation の遅延 retire / Drop は、既に公開された replacement `current.json` と socket を保持する。
- [x] accept loop は shutdown を観測して終了し、daemon owner が join してから listener の generation-fenced retire / Drop を完了し、その成功後だけ exact record を clear する。
- [x] shutdown signal handler / wait の準備が endpoint publish / worker spawn より前であることを usecase ordering test と実 process testで固定する。
- [x] transport と daemon lifecycle の v2 正本 docs を実装へ整合する。
- [ ] running/stale stop と owner cleanup が遅延しても、先に保存された replacement の完全な record を削除しない。
- [ ] record save と conditional clear の比較・unlink は stable な同一 cross-process lock 下で不可分に実行する。
- [ ] record save の途中失敗・crash は旧 record を malformed JSON にせず atomic replacement を保証し、返却された write / rename error では temporary rollback を試みて cleanup failure も報告する。
- [ ] endpoint bind 後に ordinary error を返す全経路は、公開前の temporary / generation socket の rollback を試みて cleanup failure も報告する。
- [ ] endpoint 初期化中に shutdown signal を受けても、同期 wait 前から admission fence が立ち、新規 request を受理しない。

## 必須回帰テスト

- `tests/cli_tui.rs` の実 Unix / production binary lifecycle testで、owned foreground serve 起動 → endpoint ready → stop → PID 消滅 → locator/socket 消去 → client autostart / 新 generation を検証する。
- Unix transport testで old generation 公開 → replacement generation 公開 → old retire の順を作り、replacement locator / socket / connectability が保持されることを検証する。
- Unix transport failure injection で bind 後の各 ordinary error boundary と locator publish failure が temporary / generation socket を残さないことを検証する。
- serve lifecycle fakeで prepare / publish / wait / stop+join / retire / exact record clear の順序と cleanup failure、running stop の completion wait を検証する。
- Barrier で old record load 後に replacement save を先行させる実 filesystem concurrency test、running/stale stop race、owner late clear を固定し、同じ pid でも異なる `started_at` を保持することを検証する。
- atomic save の rename 前 failpoint で旧 record と `0600` mode が保持され、失敗 temporary が残らないことを検証する。
- 隔離 subprocess で SIGTERM handler が同期 wait 前に shared admission fence を立てることを検証する。
- 既存 bootstrap testの non-absence failure で start 回数 0 を維持する。

## docs / gate

`document/04-ipc.md` の Unix transport と `document/05-daemon.md` の process lifecycle / data directory を更新する。Rust / process / signal / thread / Unix IO に影響するため、selected tests、fmt、workspace check / clippy を実行し、full test / coverage 100% は PR CI の必須 gate とする。
