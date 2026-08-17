---
number: 692
title: feat(cli): doctor で production mode の v1 data directory 共有を警告する
status: todo
priority: low
labels: [cli]
dependson: []
related: [690, 691]
created_at: 2026-08-17T22:49:33.823144+00:00
updated_at: 2026-08-17T22:49:33.823144+00:00
---

設計は [document/proposals/16-v1-v2-coexistence.md](../../document/proposals/16-v1-v2-coexistence.md) の
「設計 3: production mode の重なりを doctor で可視化する」が正本。

## 背景

v2 の runtime mode は既定 `local` で、global / project の runtime state は
`<base>/local/` と `<repo>/.usagi/local/` に分離されている。`USAGI_RUNTIME_MODE=production` を
**明示した**ときだけ `<base>/` と `<repo>/.usagi/` を直接使い、v1 の `workspaces.json` /
`settings.json` / `state.json` と同じ path が重なる。

**起動は拒否しない**。v2 が v1 を置き換えるときの正規経路がまさに production であり、そこを
fail-closed にすると cutover 自体を塞ぐ。代わりに `usagi doctor` の診断項目として可視化する。

## やること

- `usagi doctor` に診断項目を 1 つ追加する。
  - mode が production で、かつ `<base>` に v1 だけが書く同階層の痕跡（`agent-prompts/` /
    `agent-state/` / `open-panes/` / `unite-set.json` のいずれか）が同居していれば、v1 と同じ
    data directory を共有していることを警告し、`USAGI_RUNTIME_MODE` の指定方法を案内する。
  - mode が production 以外なら何も出さない。
- 判定は path の存在確認だけで、v1 の file を読まない・書かない。
- 存在確認は real IO なので port として注入し、doctor の判定 usecase は fake で全分岐をテストする。

## テスト

`cargo test -p usagi-cli`（doctor の判定 usecase）: production × 痕跡あり / production × 痕跡なし /
local / development の 4 分岐。
