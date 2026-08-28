---
number: 719
title: fix(daemon): cold start が cwd を無条件に adopt し、稼働中の判定と食い違う
status: done
priority: medium
labels: [v2, daemon, workspace, consistency]
dependson: []
related: [712, 714]
created_at: 2026-08-24T00:44:18.857349+00:00
updated_at: 2026-08-25T23:59:11+00:00
---

## Finding

`usagi session create` を **同じ directory** で実行しても、daemon が動いているかどうかで結果が変わる。

```text
# daemon が別 workspace を serve 中 → 拒否（handshake の bound 解決）
$ cd ~/projects/other        # git repository ではない
$ usagi session create x
daemon unavailable [permission_denied; error_id=workspace-mismatch]:
… is not a workspace this daemon has open; run this from a repository root …

# daemon が 1 つも動いていない → 成功し、その directory を adopt する
$ usagi daemon stop
$ cd ~/projects/other
$ usagi session create x
accepted operation …
$ find ~/projects/other/.usagi
  .usagi/daemon/daemon.lock
  .usagi/sessions/x
```

cold start では client が daemon を auto start し、その daemon は
`bound_workspace_root(startup cwd)` を initial tenant にする。ここには handshake 側の
「bound で開けるのは repository root だけ」という判定が掛からない。

結果として、利用者から見えない daemon の生死で同じ command の意味が変わる。git repository で
ない directory に `.usagi/` を作って fence を取ってしまうのも、cold start のときだけである。

## 経緯

もともと handshake 側は bound の miss を一律拒否していたため、この非対称は「片方が常に拒否」で
覆い隠されていた。#1533 で bound からの adopt を許した（repository root に限定）ことで、2 つの
経路の判定が食い違っていることが表面化した。cold start 側の挙動自体は #1533 以前からのものである。

## 修正方針

どちらかへ寄せる。

- **A. cold start にも同じ判定を掛ける**: auto start する client が、起動 cwd を initial tenant に
  する前に handshake と同じ規則で判定する。`usagi open <path>` と TUI の明示操作
  （`selected`）は従来どおり無制限のままにする。
- **B. 判定を消して cold start に合わせる**: bound でも cwd を無条件に adopt する。
  ただし `$HOME` に dotfiles repository を置く構成で `~/.usagi/sessions/<name>` を作ってしまう
  ため、上位探索を伴う形では採れない（#1533 のレビューで実測した）。

A が本命である。判定の正本は [4. IPC#workspace fence](../../document/04-ipc.md#workspace-fence)。

## 受入条件

- [x] 同じ directory・同じ command が、daemon の生死によらず同じ結果になる。
- [x] `usagi open <path>` と TUI の Open / New は repository でない directory も従来どおり開ける。
- [x] cold start が repository でない directory に fence と `.usagi/` を作らない。
- [x] 既に adopt 済みの workspace の配下からの実行は、どちらの経路でも従来どおり解決される。
