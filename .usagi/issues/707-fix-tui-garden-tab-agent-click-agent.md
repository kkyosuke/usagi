---
number: 707
title: fix(tui): Garden のうさぎを tab のある Agent だけにし、click でその Agent を開く
status: done
priority: medium
labels: [tui, garden, bug, v2]
dependson: []
related: [674, 687, 701]
created_at: 2026-08-19T23:56:38.142519+00:00
updated_at: 2026-08-20T00:13:47.180527+00:00
---

## 概要

Session Garden に、**既に閉じた Agent のうさぎが残る**。利用者から見ると「Closeup の tab には無いものが庭に居る」状態で、押しても開けるものが無い。

原因は membership の出どころである。庭と sidebar の agent 行が読む投影は、controller の runtime-local phase（session が生きている限り積み上がり、`Ended` / `Exited` を観測しても entry は消えない）に Agent inventory を重ねたもので、inventory 側も `exited` / `reclaimed` / `unavailable` をそのまま `done` / `interrupted` のうさぎへ写していた。Closeup の tab strip は同じ inventory の `live`（+ `reserved`）と `interrupted` だけを tab にするため、庭の羽数と開ける tab が食い違う。

あわせて、うさぎの click 粒度を決める。hitbox は区画（session）単位で、1 区画に複数のうさぎが居ても遷移先は session の Closeup までだった（[proposals/15](../../document/proposals/15-session-garden.md) の「決めていないこと」）。1 うさぎ = 1 Agent なのだから、押したうさぎの Agent tab へ入るのが自然である。

## 変更内容

- **membership の権威を最新 coherent Agent inventory にする**。`reserved` / `live` / `interrupted` が保持する runtime だけがうさぎ 1 羽になり、`exited` / `reclaimed` / `unavailable` は庭にも sidebar の記号にも出さない。inventory が保持する runtime の phase は、より精密な runtime-local phase を優先する（従来どおり）。inventory を 1 度も観測していない間の挙動は変えない。
- **うさぎ 1 羽ごとの hitbox を返す**。renderer が描画と同じ layout 計算で `AgentRuntimeId` 付き rectangle を返し、区画の rectangle より先に並べる。session そのものの pose（`creating` / `deleting` / `failed` と PR merge の celebration）は Agent に対応しないので `AgentRuntimeId` を持たない。
- **うさぎの click はその Agent の tab を開く**。`GardenClick::Visit { session, agent }` を運び、reducer の activation は変えないまま、shell が訪問先 Closeup で対応する tab を stable identity（live tab は terminal incarnation、中断 tab は会話 lineage）で選ぶ。一致する tab が無ければ session の Closeup に留める。

## テスト・確認方法

- `cargo test -p usagi-tui` / `cargo test --workspace --quiet`
- coverage 100%（`coverage_report --fail-under-lines 100 --fail-under-functions 100`）
- `cargo run -p usagi-tui --example garden_sample` で絵が変わっていないこと
