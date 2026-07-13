---
number: 296
title: fix(env): Env modal の mode を workspace 設定へ永続化し daemon launch へ反映する
status: done
priority: high
labels: [env, tui, daemon, persistence]
dependson: []
related: [244, 263, 264, 268, 271]
created_at: 2026-07-13T12:25:43.605504+00:00
updated_at: 2026-07-13T12:26:19.378853+00:00
---

## 背景

v1 の `env` overlay は workspace-local `SecretEnv`（`NAME = op://…`）を read-modify-write で保存し、effective settings を launch 直前に解決して PTY child へだけ渡した。scope は global defaults + workspace override であり、session scope は持たない。

v2 #244 は Environment overlay の draft、`LoadEnvironment` / `SaveEnvironment` effect、表示だけを追加したが、environment の domain value、永続化、effect runner、daemon request/refresh、launch resolver が未接続である。そのため保存・再読込・daemon snapshot/restart・実際の terminal/Agent launch のいずれにも到達しない。現在の `EnvironmentEntry` には mode metadata もなく、v1 に特別な mode enum は存在しないため、既存保存データを壊さず typed vocabulary と compatibility policy を定義する必要がある。

## 目的

Env modal で確定した environment binding とその mode を、v1 の global + workspace override contract を維持して durable に保存し、daemon-owned terminal/Agent launch に安全に反映する。TUI は daemon/store/secret を直接扱わない。

## スコープ

- v1 の global default + workspace-local override を正本として environment domain/schema を追加する。session scope を新設せず、root/session target は workspace identity に正規化する。
- binding metadata の typed mode と validation を domain に置く。未知/旧 mode、metadata 欠損、旧 `NAME=op://…` record は安全な default/互換読取にし、保存時に意図しない scope・value を失わせない。
- optimistic revision / compare-and-swap（または同等の single-writer contract）で load→edit→save の競合を安全に扱い、削除済み workspace/session target と stale snapshot を safe error にする。
- modal は open 時に保存済み値を draft へ読み込み、confirm 成功時だけ保存済み snapshot へ収束する。Esc/cancel、validation error、save conflict/error は未確定 edit を launch state に適用しない。focus、selection、Switch/Closeup、session selection は維持する。
- TUI effect runner / daemon IPC を typed environment read/save/refresh に接続する。daemon snapshot/refresh と TUI 再起動後は authoritative value を再投影し、legacy `state.json` を managed session source として使わない。
- daemon の trusted resolver が launch admission 時に workspace scope の effective bindings を解決し、terminal/Agent の one-shot `SpawnProvision` にだけ具体値を渡す。raw value/secret は durable snapshot、IPC、TUI、log に出さない。既存起動済み PTY へ遡及適用しない。

## 対象外

- session-scoped environment、client-supplied env/argv/secret の IPC field、direct TUI spawn。
- v1 の旧コードや workspace/session lifecycle の再設計。
- adapter 固有の CLI flag/model 設計。

## 受け入れ条件

- workspace で mode を確定後、modal close/reopen、session selection 切替、TUI restart、daemon refresh/restart で同じ effective value/mode が表示される。
- global default と workspace override の merge/override/delete が v1 と同じ scope で働き、session は workspace value を読むだけである。
- saved binding/mode は subsequent terminal と Agent launch に反映される。一方、cancel・invalid draft・save failure/conflict・stale/deleted target は保存/launch に影響しない。
- unknown/old mode、metadata omission、legacy record、metadata/value validation failure、concurrent writes、target removal、stale/duplicate IPC completion は safe feedback になり、secret/raw error を露出しない。
- reducer、domain/store adapter、IPC/effect runner、daemon launch resolver の regression tests を追加する。少なくとも cancel、reopen、restart/refresh、scope isolation、legacy decode、conflict、deletion、launch value non-persistence を covered とする。
- `document/03-tui.md`、`document/04-ipc.md`、`document/05-daemon.md` の実装済み契約を同じ PR で更新する。

## 依存・境界

- #244 の Environment overlay state/render を再利用し、同 issue が未実装の persistence promise を完結する。
- #263/#264/#271 の daemon-owned Agent/generic terminal runtime と `SpawnProvision` secrecy contract を消費する。launch/PTY ownership は変更しない。
- #268 の managed session snapshot を target validity の正本とし、local `WorkspaceState` を managed lifecycle に書き戻さない。
