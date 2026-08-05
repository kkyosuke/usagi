---
number: 655
title: fix(core): live VT parser の CSI/SGR state を bounded に保つ
status: done
priority: high
labels: [review, v2, core, terminal, memory, security, correctness]
dependson: []
related: [524, 533, 534]
parent: 654
created_at: 2026-08-05T13:40:17.703014+00:00
updated_at: 2026-08-05T22:28:29.983865+00:00
---

## Finding（P1 memory / availability）

`usagi-core::usecase::vt_screen::VtScreen` の live parser は、checkpoint decoder が宣言する bound を入力時に強制していない。

- `csi()` は final byte が来るまで `self.params.push(...)` を無制限に続ける。一方 checkpoint は `PARAMS_MAX = 64` を超える decoder state を reject する。
- `sgr()` は reset の無い `CSI ... m` を `self.style` へ連結し続け、`print()` はその全 `String` を cell ごとに clone する。
- checkpoint decode は style table の**件数**だけを bound し、個々の style string 長を bound しない。

一時 probe で `ESC [` + `1` × 1,000,000 を feed すると live parser は `params` 1,000,000 bytes を保持し、その自己生成 checkpoint は `ParamsTooLong(1000000)` で復元不能になった。`ESC[1m` × 100,000 + `x` では 400,000-byte style を 1 cell が保持し、checkpoint round-trip は通る。この入力は daemon-owned PTY output から到達し、owner lock 内の parser、checkpoint、TUI render の CPU/memory を増幅できる。

## 修正方針

- live `csi` parser に `PARAMS_MAX` を入力時から適用し、超過 sequence は final まで discard する bounded overflow phase/state を持つ。途中までを有効 CSI として dispatch しない。
- SGR state は raw escape 列の append log ではなく、有限の canonical attribute state（または厳密に bounded な canonical SGR）として保持する。reset、bold/dim/underline/reverse、標準/256/RGB foreground/background の既存表示 parity を保つ。
- checkpoint は live parser が常に生成可能・復元可能な invariant を持つ。必要なら individual style byte bound を schema validation に追加し、`to_json_bytes` 前に oversized allocation を作らない。
- parser/state bound と checkpoint bound の定数・意味は同じ module を SSoT にする。

## 受入条件

- 1 MiB の未終端 CSI、final 付き oversized CSI、長い SGR 連鎖を feed しても retained state / checkpoint / render cost が入力長に比例して増えない。
- oversized CSI は grid/cursor/style を途中値で更新せず、次の正常 text/CSI から回復する。
- parser が生成した任意の checkpoint は `VtScreen::from_checkpoint` で復元できる。
- SGR・cursor・alternate screen・UTF-8 chunk split の既存 parity test が維持される。
- hostile checkpoint decode は従来どおり allocate 前に fail closed する。

## 根拠箇所

- `crates/core/src/usecase/vt_screen.rs`: `csi`, `sgr`, `print`, `checkpoint`
- `crates/core/src/usecase/vt_screen/checkpoint.rs`: `PARAMS_MAX`, `STYLES_MAX`, `validated_geometry`
- `crates/daemon/src/usecase/terminal.rs`: `checkpoint_within`
