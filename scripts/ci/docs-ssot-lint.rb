#!/usr/bin/env ruby
# frozen_string_literal: true

# Check the documentation boundaries that previously drifted away from the
# implementation. This deliberately validates ownership and inventories; prose
# semantics remain a review responsibility.

require "set"

root = File.expand_path(ARGV.fetch(0, File.join(__dir__, "../..")))
failures = []

read = lambda do |relative|
  path = File.join(root, relative)
  unless File.file?(path)
    failures << "missing #{relative}"
    next ""
  end
  File.read(path)
end

overview = read.call("document/01-overview.md")

def enum_variants(source, enum_name)
  lines = source.lines
  start = lines.index { |line| line.match?(/^pub enum #{Regexp.escape(enum_name)} \{$/) }
  return [] unless start

  variants = []
  hidden = false
  lines[(start + 1)..].each do |line|
    break if line == "}\n"

    hidden = true if line.start_with?("    #[command(") && line.include?("hide = true")
    match = line.match(/^    ([A-Z][A-Za-z0-9]*)(?:\s*\{|,)\s*$/)
    next unless match

    variants << [match[1], hidden]
    hidden = false
  end
  variants
end

def kebab_case(name)
  name.gsub(/([a-z0-9])([A-Z])/, '\\1-\\2').downcase
end

cli_source = read.call("crates/cli/src/cli/mod.rs")
command_groups = {
  "Command" => "usagi",
  "DaemonCommand" => "usagi daemon",
  "SessionCommand" => "usagi session"
}
expected_commands = ["usagi"]
command_groups.each do |enum_name, prefix|
  variants = enum_variants(cli_source, enum_name)
  failures << "cannot read public variants from #{enum_name}" if variants.empty?
  variants.reject { |_, hidden| hidden }.each do |name, _|
    expected_commands << "#{prefix} #{kebab_case(name)}"
  end
end

def documented_command(code_span)
  parts = code_span.split
  return nil unless parts.first == "usagi"
  return "usagi" if parts.length == 1 || parts[1].start_with?("-", "<", "[")

  command = parts.first(2)
  if %w[daemon session].include?(parts[1]) && parts[2] && !parts[2].start_with?("-", "<", "[")
    command << parts[2]
  end
  command.join(" ")
end

entry_surface = overview[/^## 入口面\n(.*?)(?=^## )/m, 1].to_s
documented_code_spans = entry_surface.lines.flat_map do |line|
  first_cell = line[/^\|\s*(.*?)\s+\|/, 1]
  first_cell ? first_cell.scan(/`([^`]+)`/).flatten : []
end
documented_commands = documented_code_spans.map do |code_span|
  documented_command(code_span)
end.compact.to_set
expected_commands = expected_commands.to_set
allowed_documented_aliases = Set["usagi hop"]

(expected_commands - documented_commands).sort.each do |command|
  failures << "document/01-overview.md is missing public CLI command `#{command}`"
end
(documented_commands - expected_commands - allowed_documented_aliases).sort.each do |command|
  failures << "document/01-overview.md documents unknown public CLI command `#{command}`"
end

cargo = read.call("Cargo.toml")
workspace_dependencies = cargo[/^\[workspace\.dependencies\]\n(.*?)(?=^\[)/m, 1].to_s
  .lines
  .map { |line| line[/^([A-Za-z0-9_-]+)\s*=/, 1] }
  .compact
  .reject { |name| name.start_with?("usagi-") }
  .to_set
conventions = read.call("document/06-conventions.md")
dependency_section = conventions[/^## 依存クレート\n(.*?)(?=^## )/m, 1].to_s
documented_dependencies = dependency_section.lines.map do |line|
  line[/^\|\s*`([^`]+)`\s*\|/, 1]
end.compact.to_set
(workspace_dependencies - documented_dependencies).sort.each do |dependency|
  failures << "document/06-conventions.md is missing workspace dependency `#{dependency}`"
end
(documented_dependencies - workspace_dependencies).sort.each do |dependency|
  failures << "document/06-conventions.md documents stale workspace dependency `#{dependency}`"
end

numbered_docs = Dir.glob(File.join(root, "document/[0-9][0-9]-*.md")).sort
index = read.call("document/README.md")
numbered_docs.each_with_index do |path, position|
  basename = File.basename(path)
  content = File.read(path)
  failures << "document/README.md does not list #{basename}" unless index.include?("(#{basename})")
  contents_labels = content[/^## 目次\n(.*?)(?=^## )/m, 1].to_s.lines.map do |line|
    line[/^- \[(.*?)\]/, 1]
  end.compact
  body_headings = content.lines.map do |line|
    line[/^## (.+)$/, 1]
  end.compact.reject { |heading| heading == "目次" }
  unless contents_labels == body_headings
    failures << "#{basename} top-level contents do not match body heading order"
  end
  if content.match?(/\]\((?:proposals\/|\.\.\/\.usagi\/issues\/)[^)]+\)\s*が正本/)
    failures << "#{basename} makes proposal or issue history the current specification authority"
  end
  content.split(/\n{2,}/).each do |paragraph|
    next unless paragraph.include?("../.usagi/issues/")
    next if paragraph.match?(/設計経緯|実装履歴|履歴参照|完了済み/)

    failures << "#{basename} uses issue history as current specification authority"
  end
  if content.lines.length > 300 && !content.include?("## この文書の読み方")
    failures << "#{basename} exceeds 300 lines without a reading map"
  end

  if position.positive?
    previous = File.basename(numbered_docs[position - 1])
    failures << "#{basename} breadcrumb is missing previous document #{previous}" unless content.lines.first(8).join.include?("(#{previous})")
  end
  if position < numbered_docs.length - 1
    following = File.basename(numbered_docs[position + 1])
    failures << "#{basename} breadcrumb is missing next document #{following}" unless content.lines.first(8).join.include?("(#{following})")
  end
end

rust_sources = [
  *Dir.glob(File.join(root, "src/**/*.rs")),
  *Dir.glob(File.join(root, "crates/**/*.rs"))
]
rust_sources.each do |path|
  File.read(path).scan(%r{document/[A-Za-z0-9_./-]+\.md}).uniq.each do |reference|
    next if File.file?(File.join(root, reference))

    failures << "#{path.delete_prefix("#{root}/")} references missing documentation #{reference}"
  end
end

proposal_index = read.call("document/proposals/README.md")
proposal_files = Dir.glob(File.join(root, "document/proposals/[0-9][0-9]-*.md")).sort
proposal_files.each do |path|
  basename = File.basename(path)
  content = File.read(path)
  failures << "document/proposals/README.md does not list #{basename}" unless proposal_index.include?("(#{basename})")
  failures << "#{basename} is missing a machine-visible history status" unless content.lines.first(10).join.include?("> **Status:**")
  baseline = content.lines.first(10).join[/^> \*\*Baseline:\*\*(.*)$/m, 1]
  failures << "#{basename} is missing a machine-visible history baseline" unless baseline
  if baseline && !baseline.match?(/commit `[0-9a-f]{40}`（\d{4}-\d{2}-\d{2}）/)
    failures << "#{basename} history baseline is missing an exact origin commit and date"
  end
end
failures << "document/proposals/README.md must explain the reserved proposal number 06" unless proposal_index.match?(/(?:#|番号)\s*0?6|`06`/)

agents_index = read.call(".agents/README.md")
Dir.glob(File.join(root, ".agents/designs/*.md")).sort.each do |path|
  basename = File.basename(path)
  content = File.read(path)
  failures << ".agents/README.md does not list designs/#{basename}" unless agents_index.include?("(./designs/#{basename})")
  failures << "#{path.delete_prefix("#{root}/")} is missing a machine-visible history status" unless content.lines.first(10).join.include?("> **Status:**")
  baseline = content.lines.first(10).join[/^> \*\*Baseline:\*\*(.*)$/m, 1]
  failures << "#{path.delete_prefix("#{root}/")} is missing a machine-visible history baseline" unless baseline
  if baseline && !baseline.match?(/commit `[0-9a-f]{40}`（\d{4}-\d{2}-\d{2}）/)
    failures << "#{path.delete_prefix("#{root}/")} history baseline is missing an exact origin commit and date"
  end
end

current_markdown = [
  File.join(root, "README.md"),
  *numbered_docs,
  *Dir.glob(File.join(root, ".agents/*.md")),
  File.join(root, "crates/cli/src/mcp/guides/orchestration.md")
].uniq
{
  "usagi <path>" => "legacy positional workspace entry",
  "usagi launch <path>" => "nonexistent workspace launch command",
  "usagi issue " => "nonexistent issue CLI"
}.each do |token, description|
  current_markdown.each do |path|
    next unless File.file?(path)
    next unless File.read(path).include?(token)

    failures << "#{path.delete_prefix("#{root}/")} contains #{description}: #{token}"
  end
end

{
  "usagi <path>" => "legacy positional workspace entry",
  "usagi launch <path>" => "nonexistent workspace launch command",
  "usagi issue " => "nonexistent issue CLI"
}.each do |token, description|
  rust_sources.each do |path|
    next unless File.read(path).include?(token)

    failures << "#{path.delete_prefix("#{root}/")} contains #{description}: #{token}"
  end
end

delegate_source = read.call("crates/cli/src/mcp/tools/session.rs")
delegate_block = delegate_source[/pub struct SessionDelegateBrief;(.*?)(?=^pub struct |\z)/m, 1].to_s
delegate_schema = delegate_block[/fn input_schema\(&self\).*?r#"(.*?)"#/m, 1].to_s
guide = read.call("crates/cli/src/mcp/guides/orchestration.md")
schema_accepts_existing_id = delegate_schema.include?('"id"')
guide_rejects_existing_id = guide.include?("既存 agent の `id` は指定できない")
descriptor_rejects_existing_id = delegate_block.include?("既存 agent の id は指定できない")
if !schema_accepts_existing_id && !guide_rejects_existing_id
  failures << "orchestration guide does not explain that session_delegate_brief rejects an existing agent id"
end
if !schema_accepts_existing_id && !descriptor_rejects_existing_id
  failures << "session_delegate_brief descriptor does not explain that its schema rejects an existing agent id"
end
if schema_accepts_existing_id && guide_rejects_existing_id
  failures << "orchestration guide rejects an existing agent id that the session_delegate_brief schema accepts"
end
if schema_accepts_existing_id && descriptor_rejects_existing_id
  failures << "session_delegate_brief descriptor rejects an existing agent id that its schema accepts"
end

create_block = delegate_source[/pub struct SessionCreate;(.*?)(?=^pub struct |\z)/m, 1].to_s
unless create_block.include?("worktree 作成と lifecycle store 更新が完了してから応答する")
  failures << "session_create descriptor must state that worktree and lifecycle completion precede its response"
end
unless guide.include?("store の更新は daemon 内で同期的に完了してから応答する")
  failures << "orchestration guide must state that session_create completes synchronously"
end

if failures.empty?
  puts "docs-ssot-lint: ok (#{numbered_docs.length} current specs, #{proposal_files.length} proposals, #{workspace_dependencies.length} dependencies)"
  exit 0
end

warn "docs-ssot-lint: #{failures.length} problem(s)"
failures.each { |failure| warn "- #{failure}" }
exit 1
