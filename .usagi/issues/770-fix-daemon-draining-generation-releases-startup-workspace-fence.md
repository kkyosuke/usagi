---
number: 770
title: "fix(daemon): draining 世代が起動 workspace の fence を返さない"
status: done
priority: high
labels: [v2, daemon, lifecycle, workspace, correctness]
dependson: []
related: [559, 709, 712]
created_at: 2026-09-25T00:00:00+00:00
updated_at: 2026-09-25T00:00:00+00:00
---

## 問題

v4.8.0 → v4.8.3 の generation rollover 後、旧 daemon (pid 2931) が `draining` のまま残り続け、TUI の起動画面から
当該 workspace を開くと `another daemon already owns this workspace (unverified owner pid hint 2931)` で拒否された。
その workspace は session 0 件で、旧 daemon がそこで何かを serve していたわけではない。

直接の引き金は、旧 daemon の終了シーケンスが `wait_until_serving` の
`durable Agent restart recovery did not complete within 30 seconds` で失敗し、live な Agent session を抱えたまま
draining 世代として生存し続けたことだが、これ自体は planned replacement の設計どおりである（draining 世代は
自分が所有する PTY を置き換えの向こう側まで serve する）。問題は、その process が **もう serve しない workspace の
fence まで握り続ける**ことにある。

## 根拠

`serve` が取得する起動 workspace（initial tenant）の fence `<ws>/.usagi/daemon/daemon.lock` は **process 寿命に固定**で、
遊休 sweep の対象外である。`TenantRegistry::adopt_initial` は `fence: None` で登録し、`retire_idle` は
`entry.fence.is_some()` の entry しか候補にしない（fence が process のものなので、entry を落としても workspace が
返らないため）。結果として、draining 世代 — read と自分の terminal だけを admit し、新しい仕事を起こす request は
すべて拒否する状態 — でも起動 workspace だけは process が生きている限り永久に fence され、新しい active generation を
含むどの daemon も adopt できない。

## 方針

同じ 30 秒 sweep が、authority を durable に手放した generation の起動 workspace も返す。

| 条件 | 理由 |
|---|---|
| handoff が durable になっている（`AdmissionGate::handed_off`） | role が `draining` であることでは足りない。`enter_draining` は registry commit の**前**に barrier を閉じ、commit しなければ `abort_draining` で `active` へ戻る。しかもその窓は `--restart-agents` の guard が live Agent を止める区間そのもので、起動 workspace が遊休に見える区間でもある。role だけで返すと、`active` に戻った process が自分の起動 workspace を失う |
| `WorkspaceActivity::has_work` が false（fail-closed） | 遊休 sweep と同じ観測。live な generic terminal / Agent / supervisor / 未完了 lifecycle work があれば、あるいは観測できなければ保持する |
| 単一インスタンス lock が同じ descriptor を共有していない | `$USAGI_HOME` が workspace 配下にある `ProcessInstanceLock::WorkspaceAlias` のケースは singleton lock と同一 inode なので、返すと 2 つ目の daemon が起動できてしまう |

遊休 sweep の「registry の外に保持者が居ない」は条件にしない。起動 workspace の tenant handle は
workspace を申告しない接続のために process 寿命で保持されるため、その問いの答えは恒久的に「居る」であり、
条件にすると永久に返せない。代わりに「handoff が durable になった」ことが「もう新しい仕事は起きない」を保証する。

返した fence を握り直さないために、**workspace を新しく開けるのは authority をまだ手放していない generation だけ**に
する。置き換えられた世代は自分が所有する terminal を読むために自分専用の socket 経由で到達可能で、その client は
自分の cwd を申告するため、handshake の `selected` / `bound` miss がそのまま adopt になると、返したばかりの
workspace を同じ process が fence し直してしまう。

一方で handoff は「終わったあとも全参加者が draining generation に到達できる」場合だけ開始してよいので、返した
瞬間に handshake が `workspace-mismatch` になってもいけない。所有を返すことと答えることを分け、**保持中の tenant
と、返した起動 root（保持し続ける tenant handle 経由）には答え、それ以外は拒否する**。判定は 1 回の読みで行い、
「保持している」を見てから `adopt` する二段読みにはしない（その間に sweep が返すと `adopt` が fence を取り直す）。

## 受入条件

- draining 世代の起動 workspace が、仕事が残っていなければ tenant ごと返され、別 process が fence を取得できる。
  ✅ 合成ルートの test（実 fence の acquire → release → 後続 owner の acquire を観測）
- `active` の間は返さない。draining でも仕事が残っている（観測できない場合を含む）間は返さない。
  ✅ 同 test と、観測が失敗する runtime を使った test
- 単一インスタンス lock が fence を alias している場合は返さない。✅ `ProcessInstanceLock` の alias test
- 解放は高々 1 回で、以後の sweep は何もしない。✅ registry の unit test と合成ルートの test
- pre-commit barrier（`draining` だが commit 前）では返さず、abort で `active` へ戻れる。
  ✅ `AdmissionGate::handed_off` の unit test と合成ルートの test（barrier → abort → commit の順に観測）
- 置き換えられた世代の handshake は、保持済み workspace と返した起動 root に答え、それ以外を adopt しない。
  ✅ 合成ルートの resolver test（retire 後も同じ root が解決されること、`connection_workspace` も同様）

## 暫定回避（本 issue 解決前のビルド）

残留 daemon を kill する（SIGTERM は効かず SIGKILL が必要）。bootstrap broker も同時に停止する。

## 残る穴

昇格した standby は workspace fence を持たないまま起動 workspace を serve する。本 issue の解放はその穴が開く
時刻を早めるだけで作り出してはいないが、窓は広がる。#771 で扱う。
