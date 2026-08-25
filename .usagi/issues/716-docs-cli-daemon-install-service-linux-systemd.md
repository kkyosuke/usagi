---
number: 716
title: docs(cli): daemon install-service のヘルプに Linux systemd を書く
status: todo
priority: low
labels: [v2, cli, docs, daemon]
dependson: []
related: []
created_at: 2026-08-23T23:21:39.680915+00:00
updated_at: 2026-08-25T22:25:22.537811+00:00
---

## Finding

`usagi daemon install-service --help` / `uninstall-service --help` が「macOS `LaunchAgent` を install する」としか書いていない。

systemd user unit は `src/runtime/systemd.rs` に実装済みで、README と `document/05-daemon.md#service-supervision` も両対応と説明している。clap の doc comment だけが取り残されており、Linux 利用者が `--help` を読んで「使えない」と判断する。

```text
$ usagi daemon --help
  install-service    macOS `LaunchAgent` を install する
  uninstall-service  macOS `LaunchAgent` を uninstall する
```

## 修正方針

`crates/cli/src/cli/mod.rs` の 2 つの doc comment を、macOS では LaunchAgent、Linux では systemd user unit を登録する旨に直す。プラットフォームごとの詳細は
[5. daemon#service-supervision](../../document/05-daemon.md#service-supervision) が正本なので、ヘルプは 1 行に留める。

## 受入条件

- [ ] `usagi daemon --help` が macOS / Linux の両方を示す。
- [ ] `document/05-daemon.md#service-supervision` を正本として重複記述を増やさない。
- [ ] CLI の help fixture test があれば同じ変更で更新する。

## 経緯

v3.0.0 のリリースレビューで検出。実害は表示のみのため、リリースブロッカーからは外して TODO として残す。
