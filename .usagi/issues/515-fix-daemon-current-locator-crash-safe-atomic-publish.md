---
number: 515
title: fix(daemon): current locator の crash-safe atomic publish を復旧可能にする
status: in-progress
priority: high
labels: [review, v2, daemon, ipc, recovery, security, durability]
dependson: []
related: [216, 341, 507, 513, 514]
created_at: 2026-07-22T00:07:06.965391+00:00
updated_at: 2026-07-22T11:02:35+00:00
---

## 問題・影響

v2 Unix transport の `write_locator` は固定 `.current.json.tmp` を `create_new` し、write / fsync / rename failure または process crash 後に temporary file を回収しない。残骸が一つあるだけで以後の `SecureUnixListener::bind` と daemon autostart が毎回 `AlreadyExists` となり、利用者が手動削除するまで復旧できない。

さらに `OpenOptionsExt::mode(0o600)` は umask の影響を受ける。restrictive umask では publish 後の `current.json` が exact `0600` にならず、client の ownership/mode 検証に拒否される。locator 検証は symlink 拒否だけで regular file を保証していない。

`SecureUnixListener::bind` は Unix socket node を作成してから locator publication が完了するまで owner object を構築しない。socket の chmod / rename / verify / nonblocking 化、locator の write / sync / rename / final verify のいずれかで失敗すると custom `Drop` による cleanup を利用できず、`.sock.bind`、generation socket、writer temporary、または closed socket を指す `current.json` を残し得る。replacement publish の失敗は既存 locator の bytes と接続可能性を保持し、試行が所有する全 artifact だけを明示 rollback しなければならない。

合成ルートの `acquire_bootstrap_lock` は `bootstrap.lock` を plain `create/open` し、mode、fd-based chmod、file type、effective UID、link count、symlink / descriptor inheritance を検証しない。umask `0777` では初回 client が作成 fd を lock できても mode `000` の node が残り、次回 client が reopen できず bootstrap が永久に失敗する。

また `daemon.json` を endpoint retirement の completion fence とする契約が failure recovery で閉じていない。owner の retire / Drop failure 後に stale `stop` を再実行すると current locator と socket の cleanup を証明せず record だけを clear できる。abnormal startup、accept-loop panic、または `IpcReady` の worker / listener ownership loss でも、empty state を retirement success と誤認して record を clear し、closed locator が後続 bootstrap を永久に阻害し得る。

cleanup failure で record を保持しても、後続の ordinary `start` / LaunchAgent `serve` が singleton lock 取得直後にその stale record を上書きすると completion fence を迂回する。新 owner は pre-registration recovery で旧 endpoint の不在を証明するまで record を置換してはならない。accept worker が panic しても main の signal wait は wake しないため、明示 signal が来るまで stale endpoint / record / singleton lock を保持し続ける経路もある。

polling client の `connect_current` が未起動の `daemon/` を作成すると、mkdir の umask-derived mode と chmod の間を startup owner が観測し、正当な初回起動を unsafe mode として拒否する競合になる。stale generation scan も `generations/` root を検証しなければ symlink 経由で daemon directory 外の socket を削除し得る。

同じ create→fd-fchmod crash window は `current.lock`、`daemon.lock`、`record.lock` にも存在する。umask `0777` 下で create 直後に SIGKILL されると mode `000` node が残り、次回 open が失敗して locator、instance lifecycle、record transaction を永久に停止する。private directory も mkdir→chmod の途中で concurrent process に transient mode を拒否され、mode `000` crash residue を修復できない。

lock path の検証と opened fd の検証が別々でも、open / flock の間に pathname を unlink・再作成されると旧 inode と新 inode を別 process が同時に lock できる。flock 取得後に pathname と fd の device / inode が一致し、regular file、effective UID、single link、exact mode を同時に満たすことを証明しなければ publication / retirement / record / singleton fence は成立しない。

daemon が SIGKILL された場合は `current.json` と generation socket が残り、通常 client の接続は `ConnectionRefused` になる。しかし client bootstrap は locator 自体が `NotFound` の場合だけ lifecycle `start` を実行し、それ以外の接続失敗を fail closed にしているため、serve 側へ追加した pre-registration stale recovery に到達できない。接続エラーだけを stale の根拠にしてはならない一方、ordinary CLI / TUI bootstrap が `bootstrap.lock` を保持し、authoritative な daemon instance lock を取得して active usagi owner の不在を証明し、whole lifecycle record の exact recheck を通過した場合は、手動 stop/start なしに endpoint を retire して replacement を一つだけ起動できなければならない。malformed / unsafe locator、record ownership unknown、busy instance owner は引き続き state を保持して fail closed とする。この recovery は raw PID を probe / signal しない。

`NotFound` も発生位置で意味が異なる。locator 自体が存在しない場合は未起動 state として通常の cold start に進める。一方、secure に検証済みの locator が指す endpoint socket が socket-first retirement の途中ですでに消えている場合、または endpoint 検証後から `connect` までの間に消失した場合の `ENOENT` は、published endpoint の到達不能であって locator absence ではない。これを raw `NotFound` のまま bootstrap へ返すと、daemon instance / whole-record proof を迂回した無条件 `start` に入り、cleanup 中の live owner や PID が再利用された旧 record と競合して duplicate daemon を起動し得る。したがって locator 検証後の endpoint `ENOENT` は `ConnectionRefused` 相当の recoverable-unavailable class に正規化し、socket-first partial retire も必ず同じ stale recovery proof を通す必要がある。

## 成立条件・再現

着手時の指定基点 `e047610b` の未修正実装に、private daemon directory 内へ既存 `.current.json.tmp` を置いてから locator publish する回帰テストを先行追加した。次の command は `Os { code: 17, kind: AlreadyExists }` で失敗し、固定 orphan が後続 publication を阻害することを確認した。#513 merge 後は `origin/main 62f2a65c` へ rebase して同じ root cause の残存範囲を再監査した。

`cargo test -p usagi-daemon pre_existing_orphan_locator_temp_does_not_block_publication -- --nocapture`

## 重複・依存監査

既存 issue store を `current.json` / `.current.json.tmp` / locator / stale recovery / Unix transport で検索し、本件を扱う open issue はなかった。監査時点の open PR #1225（issue #513、head `5f704131`）は planned stop 時の endpoint retire と `current.lock` による publish/retire 直列化を扱うが、issue 本文で panic/crash stale recovery を scope 外としている。同 head にも固定 temp、failure cleanup、umask 問題は残るため非重複である。

本件は `origin/main` から独立して修正可能なので #513 を blocking dependency にはせず related とする。writer ordering / late stale generation の replacement fence は #513 の `current.lock` が正本であり、本件では古い generation socket や `current.json` を推測削除しない。#1225 rebase 時は corrected publish を lock の内側に置き、新規 `current.lock` にも同じ secure file create/verification primitive を適用できる構造にする。

作業中に #1225 は main `62f2a65c` として merge された。rebase では generation-fenced retire と bind rollback を保持し、corrected publication を `current.lock` の内側へ統合した。fresh locator temp と既存 `current.lock` は共通の fd-based regular file / effective UID / exact mode / single-link 検証を使う。

#513 が所有する planned-stop 正常系と通常の bind-stage rollback の意味論は重複変更しない。本 issue は #513 が deferred した crash / panic / cleanup-failure recovery と、main に残る locator publication / `bootstrap.lock` を扱う。追加監査で判明した同一 root cause を閉じるため、#513 が導入した `current.lock` / `daemon.lock` を含む全 lifecycle lock node の作成・再open security invariant は本 issue で一貫して harden する。stale recovery は exact generation と endpoint で fence し、concurrent replacement の locator / socket / record を削除しない。

## 修正方針

- 固定 temp を writer ごとに一意な daemon-directory 内 private temp へ置換する。
- fresh temp は `create_new | O_NOFOLLOW | O_CLOEXEC` で開き、fd を `fchmod(0600)` してから regular file / effective UID owner / exact mode を fd metadata で検証する。
- bytes の write と file fsync 後だけ `current.json` へ atomic rename する。rename 前の create / write / sync / rename failure は既存 locator を保持し、その試行が所有する temp だけを必ず cleanup する。
- fchmod / fd 検証済みの temp inode だけを final locator へ rename し、discovery は final locator を secure-open した同じ fd 上で再検証する。parent directory fsync は post-commit ambiguity を error にしない best-effort とする。
- bind 後の全 ordinary failure boundary で試行所有権を追跡し、`.sock.bind`、generation socket、locator temporary と、公開済みなら exact new locator を rollback する。replacement failure は old locator の exact bytes と socket connectability を保持する。
- `bootstrap.lock` は `create_new` / secure reopen の両経路を `O_NOFOLLOW | O_CLOEXEC` で開き、fd 上で regular file、effective UID、`nlink == 1` を検証して `fchmod(0600)` し、exact mode を再検証してから lock する。
- stale stop は owned socket を先に回収し、exact locator の conditional removal を commit fence として、その cleanup proof 後だけ exact daemon record を clear する。absent locator も owned socket の不在を証明できた場合だけ成功とし、replacement generation は保持する。
- startup / accept-loop / retire の失敗後も endpoint cleanup capability を失わず、retryable cleanup が current と socket の回収を証明するまで daemon record を保持する。
- `serve` は singleton lock 取得後かつ新 record 保存前に stale endpoint recovery を必ず実行し、recovery 前後の old record exact match を確認してからだけ新 incarnation を保存する。ordinary start と LaunchAgent restart の両方で cleanup fence を迂回しない。
- accept worker の exit guard で shared shutdown fence を立て、main wait が OS signal なしでも worker panic / loss を観測して cleanup state machine へ進む。
- discovery は read-only とし、polling client が daemon directory を作成しない。stale scan は `generations/` root の owner / mode / symlink invariant を検証してから descendant socket を回収する。
- post-rename verify failure の rollback は final path が prepared inode のままの場合だけ old locator を復元し、別 inode の replacement は保持する。restore failure 後も writer 所有 temp を全回収する。
- `current.lock` / `daemon.lock` / `record.lock` / `bootstrap.lock` は create と reopen を共通の secure invariant で扱い、owner single-link regular mode `000` crash residue を trusted private parent 内だけで `0600` へ修復する。create fd は `fchmod(0600)` 後に検証する。
- flock 取得後に path を `lstat` し、fd と path の device / inode、type、owner、`nlink == 1`、exact mode を再検証する。swap / recreate を検出した取得は effect を実行せず fail closed とする。
- private directory は creation mode を最初から `0700` に制限し、umask が削った owner bitsだけを、trusted parent・same inode・owner directory の証明後に修復する。simultaneous first boot と create→chmod crash の両方を idempotent に収束させる。
- locator 自体を secure read する前の `NotFound` だけは未起動 state として通常の `start` path に進める。locator の検証後に endpoint の検証が `ENOENT` になった場合と、検証済み socket が `connect` 前に消失した場合は `ConnectionRefused` 相当の published-endpoint-unavailable class に正規化し、無条件 start ではなく stale recovery へ送る。malformed / unsafe locator と endpoint の unsafe metadata error は正規化せず fail closed を維持する。
- ordinary client bootstrap の validated locator への `ConnectionRefused` または上記 endpoint-disappearance class は、`bootstrap.lock` 保持中に exact daemon record を snapshot し、daemon instance lock を取得して active usagi owner の不在を証明し、同じ whole record を再検証した場合だけ endpoint cleanup と exact record clear を行う。cleanup 完了後に instance lock を解放してから通常の `start` path へ進み、replacement daemon の二重起動を防ぐ。instance lock が busy なら state を変更せず bounded reconnect する。
- raw PID / signal-0、接続エラー単体、または socket-first partial retire による endpoint absence を stale authority にせず、この recovery から process signal を送らない。cleanup 中の live owner と PID-reused record は daemon instance lock + whole-record fence で区別する。現行 `(pid, started_at)` は OS process identity ではなく durable incarnation fence である。#514 の exact process identity field が統合された後も、一部 field でなく `DaemonRecord` 全体の equality / conditional clear を維持する。
- pre-existing fixed orphan は後続 publish を阻害しないが、所有権を推測して削除しない。
- late writer ordering、旧 generation socket/current の推測 cleanup、planned-stop retire は変更しない。

## 受入条件

- [x] pre-existing `.current.json.tmp` orphan があっても新 locator を publish できる。
- [x] write / sync / rename の各 injected failure で old `current.json` の bytes/locatorを保持し、当該 writer temp を残さない。
- [x] 各 failure 後の retry が成功し、成功時にも writer temp leak がない。
- [x] restrictive umask 下でも fresh temp と final `current.json` が symlink でない regular file、effective UID owner、exact `0600` になる。
- [x] temp open は `O_NOFOLLOW | O_CLOEXEC` を使い、fresh fd を chmod 後に検証する。
- [x] atomic rename 後の parent fsync failure は committed publication を failure と報告しない。
- [x] concurrent writer / late stale writer の契約を #513 と競合させず、古い generation socket/current を推測削除しない。
- [x] `document/04-ipc.md` を locator publish の SSoT として更新し、`document/05-daemon.md` は data-directory entry から参照する。
- [ ] socket bind 後の chmod / socket rename / endpoint verify / nonblocking / locator publication 各 failure は、既存 locator の exact bytes と接続可能性を保持し、試行が所有する `.sock.bind` / generation socket / locator temp / new locator を残さない。
- [ ] locator final verify / hardlink failure を含む全 returned error は old locator または locator absence を保持し、retry が成功する。fresh / final locator と secure lock の `nlink == 1` invariant を維持し、hardlink を拒否する。
- [ ] `bootstrap.lock` は create / reopen とも `O_NOFOLLOW | O_CLOEXEC`、regular file、effective UID owner、`nlink == 1`、exact `0600` を満たし、fresh fd の `fchmod(0600)` により umask `0777` 下でも drop 後の再 open が成功する。
- [ ] symlink / hardlink / non-regular な `bootstrap.lock` を拒否し、既存 target や replacement を変更しない。
- [ ] retire / cleanup failure 後の stale stop retry は current locator と owned socket の cleanup proof なしに record だけを clearせず、cleanup 成功後だけ exact record を clearする。
- [ ] abnormal startup、accept-loop panic、listener retire / Drop failure でも cleanup ownership を保持し、`IpcReady` の worker / listener loss を empty-success と誤認しない。deterministic test で failure → repair → retry を固定する。
- [ ] stale cleanup と publication rollback は exact generation / endpoint / record で fence し、concurrent replacement の current、socket、record を保持する。
- [ ] `serve` は old record の snapshot → stale endpoint recovery → exact recheck 後だけ new record / endpoint を公開し、ordinary start / LaunchAgent が cleanup failure record を上書きしない。
- [ ] accept worker panic / unexpected exit は OS signal なしで main wait を wake し、join failure 後も cleanup token と record を cleanup proof まで保持する。
- [ ] polling connect は daemon directory を作成せず startup mode race を起こさない。stale scan は symlinked `generations/` root を拒否して outside socket を保持する。
- [ ] post-rename final path replacement / hardlink を deterministic failpoint で検出し、replacement / old locator を保持しつつ全 writer temp と failed generation socket を回収して retryできる。
- [ ] `current.lock` / `daemon.lock` / `record.lock` / `bootstrap.lock` の各 create→fchmod crash residue（umask `0777` の mode `000`）を次回 acquire が安全に修復し、subprocess/failpoint test で再取得できる。
- [ ] 全 lock は `O_NOFOLLOW | O_CLOEXEC`、regular/euid/nlink1/exact0600 を満たし、flock 後の path dev+ino と fd dev+ino が一致しない swap/recreate race を拒否する。barrier test で旧/new inode の split lock を effect 前に止める。
- [ ] private directory は mode-limited create と owner/inode/trusted-parent 検証付き repair により restrictive umask crash residue と simultaneous first boot を復旧し、symlink/non-owner/non-directory/unsafe broader mode を修復しない。
- [ ] locator 自体の absence は `NotFound` の通常 cold start を維持する一方、secure に検証済み locator 後の endpoint verify `ENOENT` と verify→connect 間の socket 消失は `ConnectionRefused` 相当へ分類し、bootstrap の無条件 `NotFound => start` を通らず stale recovery proof へ進む。unsafe / malformed endpoint error は分類を緩めない。
- [ ] daemon SIGKILL 後の validated locator に対する ordinary CLI / TUI bootstrap は、`bootstrap.lock` + acquired daemon instance lock + whole-record exact recheck の下だけ stale socket / locator / exact record を回収し、instance lock 解放後に fresh endpoint を一つだけ起動する。手動 lifecycle command を要求しない。
- [ ] socket-first partial retire で locator が残り endpoint だけが absent でも、live owner / PID reuse を raw PID から推測して start / signal しない。`ConnectionRefused` または endpoint `ENOENT` 単体、malformed / unsafe locator、record ownership unknown、busy owner、record replacement では stale cleanup / record clear / duplicate start を行わない。busy owner は state を保持して bounded reconnect し、bootstrap recovery は raw PID を probe / signal しない。#514 rebase 後も whole-record fence を保持する。
- [ ] `document/04-ipc.md` と `document/05-daemon.md` を durable publication / cleanup recovery と secure `bootstrap.lock` の実装へ整合する。

## 必須回帰テスト・gate

failpoint test で pre-existing orphan、write / sync / rename / final verify failure、bind 後の全 failure、old locator preservation / connectability、retry success、no owned artifact leak、hardlink rejectionを固定する。stale stop の cleanup failure → retry、abnormal startup、accept-loop panic、`IpcReady` worker / listener lossも deterministic seam で検証する。restrictive umask は process-global state を他 test と競合させない subprocess で各 lock の creation boundary と初回取得・drop・再 open を検証する。

bootstrap classification は、(a) locator 自体の `NotFound` が recovery callback なしで通常 start へ進む、(b) validated locator が指す socket の endpoint verify 時 `ENOENT`、(c) verify 後から connect 前の barrier で socket が消失する、の三経路を分離して固定する。(b) と (c) は `ConnectionRefused` 相当として daemon-lock / whole-record recovery だけに到達し、malformed / unsafe error は到達しないことを確認する。socket-first partial retire の deterministic test では current locator と record を残して owned socket だけを除去し、daemon instance lock が busy な live owner は state 保持 + bounded reconnect、lock が取得可能でも live または reused PID 値だけを stale authority にせず whole-record exact fence を通して一度だけ replacement を起動することを検証する。

実 process E2E で live daemon / endpoint → SIGKILL → manual lifecycle なしの ordinary client bootstrap → stale endpoint retirement → fresh endpoint 起動を確認し、daemon が一つだけであることを固定する。加えて socket-first partial retire で locator / record を保持したまま endpoint socket がない状態から ordinary client を起動し、raw `NotFound => start` ではなく同じ fenced recovery を経由して fresh endpoint が一つだけになることを固定する。Unix IO・永続化・security boundary の変更なので fmt、workspace check/clippy、selected/full tests、coverage 100%、Markdown link check を必須とする。
