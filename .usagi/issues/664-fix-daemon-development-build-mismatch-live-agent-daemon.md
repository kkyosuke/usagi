---
number: 664
title: fix(daemon): development の build mismatch で live Agent を破棄せず daemon を再利用する
status: done
priority: high
labels: [v2, daemon, lifecycle, tui]
dependson: []
related: [507, 528, 559, 572]
created_at: 2026-08-11T23:55:32.445020+00:00
updated_at: 2026-08-11T23:58:07.964797+00:00
---

## 問題・影響

複数の TUI を開いて Agent を起動すると、片方の TUI で開いていた Agent tab が `interrupted` になり、live として表示されなくなる。2 個目の TUI で Agent を開くと 1 個目の TUI 側の Agent が `interrupted` になる、という双方向の症状になる。

原因は client 側 bootstrap の唯一の暗黙的 daemon 再起動経路である。合成ルート（`src/runtime/daemon.rs` の `bootstrap_client`）は、development runtime mode で build artifact mismatch を検出すると `usagi daemon restart --force` を実行していた。artifact identity は source tree の content digest（`build.rs`）なので、**開発中に再 build した binary は必ず別 artifact になる**。したがって

1. TUI を開いたまま `cargo build` して 2 個目の TUI を起動すると、その bootstrap が cold transition を強制する。
2. cold transition は旧 daemon process を落とすため、旧 process が持つ PTY master は失われ、fresh daemon は未終端 runtime を `identity_unknown` へ reconcile する。TUI はこれを interrupted Agent tab として投影する。
3. 1 個目の TUI（古い artifact の process）が次の control request を出すと同じ判断で daemon を再び入れ替えるため、両者が交互に相手の Agent を落とし続ける。

実際の development data directory では、この churn が 40 秒間に 3 世代（各世代が 1 つの Agent を spawn したまま放棄）として観測された。

`--force` は #507 が **seamless rollover を production から起動できなかった時点**で選んだ override である。#559 / #572 で planned replacement の seamless rollover が配線された後は、この override が唯一 live PTY を暗黙に破棄する経路として残っていた。

## 対象責務

1. development の build mismatch は **planned replacement**（`--force` なし）で消費する。live runtime の有無は daemon 自身の census が判定し、何も live でなければ cold transition、live runtime があれば PTY を維持する gated rollover になる。
2. planned replacement が拒否された場合、または replacement 後も別 artifact が広告されている場合（= on-disk executable が client の artifact ではない場合）は、到達可能な daemon を effect 0 で再利用する。build mismatch を理由に他 client の live Agent を落とさず、かつ自分の build が既に disk 上に無い client を control request ごとの refusal で行き止まりにもしない。
3. replacement を試すのは daemon artifact ごとに 1 回だけとし、bootstrap ごとに generation を churn させない。reuse は daemon artifact ごとに 1 行だけ日次 error log に記録する。
4. 別 workspace を所有する daemon の typed refusal は、どちらの段でも replacement も reuse もせずそのまま surface する。

## 受入条件

- [x] development の build mismatch が `daemon restart` を `--force` なしで実行する。
- [x] planned replacement の失敗（live runtime の refusal）と、replacement 後の artifact 不一致は、到達可能な daemon の reuse に落ちる。
- [x] 同じ daemon artifact に対する 2 回目以降の観測は restart を実行しない。
- [x] reuse は running / expected artifact と非機密な理由を daemon artifact ごとに 1 行だけ記録する。
- [x] workspace refusal は attempt 段・reuse 段のいずれでも typed に surface する。
- [x] unit test が上記 5 つの遷移を fake endpoint で固定する。

## docs

[05-daemon.md の authority と lifecycle](../../document/05-daemon.md#authority-と-lifecycle)（build mismatch の 3 段と 1 回だけの attempt）と [04-ipc.md の identity と fence](../../document/04-ipc.md#identity-と-fence) を現在形に更新した。
