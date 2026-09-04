#!/usr/bin/env bash
set -euo pipefail

repo=$(cd "$(dirname "$0")/../.." && pwd)
subject=$repo/scripts/ci/docs-ssot-lint.rb
tmp=$(mktemp -d "${TMPDIR:-/tmp}/usagi-docs-ssot.XXXXXX")
trap 'rm -rf "$tmp"' EXIT

make_fixture() {
  local destination=$1
  mkdir -p "$destination/crates/cli/src/cli" "$destination/crates/cli/src/mcp/tools" "$destination/crates/cli/src/mcp/guides" "$destination/crates/core/src/domain/settings" "$destination/crates/core/src/infrastructure" "$destination/crates/core/src/usecase" "$destination/crates/daemon/src" "$destination/crates/tui/src/usecase"
  cp "$repo/Cargo.toml" "$destination/Cargo.toml"
  cp -R "$repo/document" "$destination/document"
  cp -R "$repo/.agents" "$destination/.agents"
  cp "$repo/README.md" "$destination/README.md"
  cp "$repo/crates/cli/src/cli/mod.rs" "$destination/crates/cli/src/cli/mod.rs"
  cp "$repo/crates/cli/src/mcp/tools/session.rs" "$destination/crates/cli/src/mcp/tools/session.rs"
  cp "$repo/crates/cli/src/mcp/guides/orchestration.md" "$destination/crates/cli/src/mcp/guides/orchestration.md"
  cp "$repo/crates/core/src/domain/settings/mod.rs" "$destination/crates/core/src/domain/settings/mod.rs"
  cp "$repo/crates/core/src/infrastructure/role_catalog.rs" "$destination/crates/core/src/infrastructure/role_catalog.rs"
  cp "$repo/crates/core/src/usecase/client.rs" "$destination/crates/core/src/usecase/client.rs"
  cp "$repo/crates/daemon/src/lib.rs" "$destination/crates/daemon/src/lib.rs"
  cp "$repo/crates/tui/src/usecase/terminal_input.rs" "$destination/crates/tui/src/usecase/terminal_input.rs"
}

expect_fail() {
  local fixture=$1 expected=$2
  if output=$(ruby "$subject" "$fixture" 2>&1); then
    echo "expected docs SSoT fixture to fail: $expected" >&2
    exit 1
  fi
  case "$output" in
    *"$expected"*) ;;
    *) echo "expected '$expected', got: $output" >&2; exit 1 ;;
  esac
}

LC_ALL=C LANG=C ruby "$subject" "$repo"

make_fixture "$tmp/dependency"
sed -i.bak '/| `syn` |/d' "$tmp/dependency/document/06-conventions.md"
expect_fail "$tmp/dependency" 'missing workspace dependency `syn`'

make_fixture "$tmp/stale-dependency"
sed -i.bak '/| `syn` |/a\
| `not-a-crate` | 古い記述 | dev |' "$tmp/stale-dependency/document/06-conventions.md"
expect_fail "$tmp/stale-dependency" 'documents stale workspace dependency `not-a-crate`'

make_fixture "$tmp/command"
sed -i.bak '/pub enum Command {/a\
    DocsProbe,
' "$tmp/command/crates/cli/src/cli/mod.rs"
expect_fail "$tmp/command" 'missing public CLI command `usagi docs-probe`'

make_fixture "$tmp/command-outside-table"
sed -i.bak '/| `usagi update /d' "$tmp/command-outside-table/document/01-overview.md"
sed -i.bak '/^## 実行モデル/i\
説明文では `usagi update` に言及する。\
' "$tmp/command-outside-table/document/01-overview.md"
expect_fail "$tmp/command-outside-table" 'missing public CLI command `usagi update`'

make_fixture "$tmp/unknown-command"
sed -i.bak '/| `usagi open \[path\]` |/a\
| `usagi frobnicate` | 存在しない command |' "$tmp/unknown-command/document/01-overview.md"
expect_fail "$tmp/unknown-command" 'documents unknown public CLI command `usagi frobnicate`'

make_fixture "$tmp/breadcrumb"
sed -i.bak '3s/(10-session-roles.md)/(missing.md)/' "$tmp/breadcrumb/document/09-env.md"
expect_fail "$tmp/breadcrumb" 'breadcrumb is missing next document 10-session-roles.md'

make_fixture "$tmp/contents"
sed -i.bak '/^- \[検討した代替案\]/d' "$tmp/contents/document/02-architecture.md"
expect_fail "$tmp/contents" '02-architecture.md top-level contents do not match body heading order'

make_fixture "$tmp/work-run-keybindings"
sed -i.bak '/| `Ctrl-O w` | WorkRuns |/d' "$tmp/work-run-keybindings/document/11-keybindings.md"
expect_fail "$tmp/work-run-keybindings" 'document/11-keybindings.md is missing implemented leader action WorkRuns'

make_fixture "$tmp/work-run-key-drift"
sed -i.bak 's/`Ctrl-O w` | WorkRuns/`Ctrl-O q` | WorkRuns/' "$tmp/work-run-key-drift/document/11-keybindings.md"
expect_fail "$tmp/work-run-key-drift" 'document/11-keybindings.md documents stale leader shortcut `Ctrl-O q` (WorkRuns)'

make_fixture "$tmp/duplicate-shortcut"
printf '\n| `Ctrl-O w` | WorkRuns |\n' >> "$tmp/duplicate-shortcut/README.md"
expect_fail "$tmp/duplicate-shortcut" 'README.md duplicates the leader shortcut table owned by document/11-keybindings.md'

make_fixture "$tmp/work-run-ipc"
sed -i.bak '/^- \[Work Run observation and control\]/d' "$tmp/work-run-ipc/document/04-ipc.md"
sed -i.bak '/^## Work Run observation and control$/d' "$tmp/work-run-ipc/document/04-ipc.md"
expect_fail "$tmp/work-run-ipc" 'document/04-ipc.md must own the implemented Work Run observation and control requests'

make_fixture "$tmp/work-run-history"
sed -i.bak 's/現在契約にない後続段階は、独立 Run Closeup/現在契約にない後続段階は、選択可能な複数 run 一覧、独立 Run Closeup/' "$tmp/work-run-history/document/proposals/18-goal-driven-work-run.md"
expect_fail "$tmp/work-run-history" 'proposal 18 classifies the implemented Work Run list as future work'

make_fixture "$tmp/team-template"
sed -i.bak '/| pipeline | `pipeline` |/d' "$tmp/team-template/document/10-session-roles.md"
expect_fail "$tmp/team-template" 'document/10-session-roles.md is missing implemented Team template pipeline'

make_fixture "$tmp/team-readme"
sed -i.bak 's/`pipeline`（パイプライン）/パイプライン/' "$tmp/team-readme/README.md"
expect_fail "$tmp/team-readme" 'README.md is missing implemented Team template pipeline'

make_fixture "$tmp/team-depth"
sed -i.bak 's/Director → Planner → Implementer → Tester | 3 |/Director → Planner → Implementer → Tester | 4 |/' "$tmp/team-depth/document/10-session-roles.md"
expect_fail "$tmp/team-depth" 'document/10-session-roles.md does not reflect built-in pipeline roles and depth'

make_fixture "$tmp/team-guide"
sed -i.bak 's/Director → Manager → Worker/Director → Manager → Executor/' "$tmp/team-guide/crates/cli/src/mcp/guides/orchestration.md"
expect_fail "$tmp/team-guide" 'orchestration guide does not use the hierarchical Team role vocabulary'

make_fixture "$tmp/history-authority"
printf '\n[古い提案](proposals/01-entry-surfaces.md) が正本である。\n' >> "$tmp/history-authority/document/09-env.md"
expect_fail "$tmp/history-authority" '09-env.md makes proposal or issue history the current specification authority'

make_fixture "$tmp/issue-authority"
printf '\n[完了 issue](../.usagi/issues/999-example.md) の契約に従う。\n' >> "$tmp/issue-authority/document/09-env.md"
expect_fail "$tmp/issue-authority" '09-env.md uses issue history as current specification authority'

make_fixture "$tmp/source-doc-link"
sed -i.bak 's#document/05-daemon.md#document/proposals/missing.md#' "$tmp/source-doc-link/crates/daemon/src/lib.rs"
expect_fail "$tmp/source-doc-link" 'crates/daemon/src/lib.rs references missing documentation document/proposals/missing.md'

make_fixture "$tmp/history"
sed -i.bak '/> \*\*Status:\*\*/d' "$tmp/history/document/proposals/17-multi-workspace-daemon.md"
expect_fail "$tmp/history" '17-multi-workspace-daemon.md is missing a machine-visible history status'

make_fixture "$tmp/baseline"
sed -i.bak '/> \*\*Baseline:\*\*/d' "$tmp/baseline/document/proposals/17-multi-workspace-daemon.md"
expect_fail "$tmp/baseline" '17-multi-workspace-daemon.md is missing a machine-visible history baseline'

make_fixture "$tmp/imprecise-baseline"
sed -i.bak 's/ea6fe2b3caa3d97f04465c7a684487f3d9a5d132/deadbeef/' "$tmp/imprecise-baseline/document/proposals/17-multi-workspace-daemon.md"
expect_fail "$tmp/imprecise-baseline" '17-multi-workspace-daemon.md history baseline is missing an exact origin commit and date'

make_fixture "$tmp/reading-map"
sed -i.bak '/^## この文書の読み方$/d' "$tmp/reading-map/document/07-mcp.md"
expect_fail "$tmp/reading-map" '07-mcp.md exceeds 300 lines without a reading map'

make_fixture "$tmp/design-index"
sed -i.bak '/designs\/258-controller-runtime-migration.md/d' "$tmp/design-index/.agents/README.md"
expect_fail "$tmp/design-index" '.agents/README.md does not list designs/258-controller-runtime-migration.md'

make_fixture "$tmp/legacy"
printf '\n`usagi issue list`\n' >> "$tmp/legacy/.agents/README.md"
expect_fail "$tmp/legacy" 'contains nonexistent issue CLI'

make_fixture "$tmp/legacy-source"
printf '\n// Legacy entry: usagi <path>\n' >> "$tmp/legacy-source/crates/daemon/src/lib.rs"
expect_fail "$tmp/legacy-source" 'crates/daemon/src/lib.rs contains legacy positional workspace entry'

make_fixture "$tmp/historical-legacy"
printf '\n設計当時は `usagi <path>` と記載していた。\n' >> "$tmp/historical-legacy/.agents/designs/258-controller-runtime-migration.md"
ruby "$subject" "$tmp/historical-legacy" >/dev/null

make_fixture "$tmp/delegate-guide"
sed -i.bak '/worker を作るため、既存 agent の `id` は指定できない/d' "$tmp/delegate-guide/crates/cli/src/mcp/guides/orchestration.md"
expect_fail "$tmp/delegate-guide" 'orchestration guide does not explain that session_delegate_brief rejects an existing agent id'

make_fixture "$tmp/delegate-schema"
sed -i.bak 's/"agent":{"oneOf":\[{"type":"object","properties":{"runtime"/"agent":{"oneOf":[{"type":"object","properties":{"id":{"type":"string"},"runtime"/' "$tmp/delegate-schema/crates/cli/src/mcp/tools/session.rs"
expect_fail "$tmp/delegate-schema" 'orchestration guide rejects an existing agent id that the session_delegate_brief schema accepts'

make_fixture "$tmp/create-description"
sed -i.bak 's/worktree 作成と lifecycle store 更新が完了してから応答する/作成は非同期に受理される/' "$tmp/create-description/crates/cli/src/mcp/tools/session.rs"
expect_fail "$tmp/create-description" 'session_create descriptor must state that worktree and lifecycle completion precede its response'

echo "docs-ssot fixtures: ok"
