#!/usr/bin/env ruby
# frozen_string_literal: true

# Validate every v2 #[coverage(off)] against the machine-readable registry.
# Ruby keeps this policy checker outside the Rust coverage measurement itself.

require "date"
require "json"
require "optparse"
require "pathname"

ALLOWED_REASONS = %w[real_io composition generic_monomorphization].freeze
DEBT_REASON = "migration_debt"
ATTRIBUTE = /^\s*#!?\[coverage\(off\)\]/
INLINE = /\/\/\s*coverage:\s*(.+)$/
IGNORED_ROOTS = %w[.git target v1].freeze

Options = Struct.new(:root, :manifest, :today, :generate, keyword_init: true)

def parse_options
  options = Options.new(root: ".", manifest: "coverage-off-allowlist.json", today: Date.today, generate: false)
  OptionParser.new do |parser|
    parser.on("--root PATH") { |value| options.root = value }
    parser.on("--manifest PATH") { |value| options.manifest = value }
    parser.on("--today YYYY-MM-DD") { |value| options.today = Date.iso8601(value) }
    parser.on("--generate") { options.generate = true }
  end.parse!
  options
rescue Date::Error => error
  warn "coverage-off-lint: invalid --today: #{error.message}"
  exit 2
end

def source_files(root)
  Dir.glob(File.join(root, "**", "*.rs")).reject do |file|
    relative = Pathname(file).relative_path_from(Pathname(root)).to_s
    IGNORED_ROOTS.include?(relative.split(File::SEPARATOR).first)
  end.sort
end

def symbol_after(lines, index, inner)
  return "<module>" if inner

  snippet = +""
  lines[index, 40].to_a.each do |line|
    code = line.sub(%r{//.*$}, "").strip
    next if code.empty? || code.start_with?("#")

    snippet << " " << code
    match = snippet.match(/\bfn\s+([A-Za-z_][A-Za-z0-9_]*)/)
    return "fn:#{match[1]}" if match
    match = snippet.match(/\bmod\s+([A-Za-z_][A-Za-z0-9_]*)/)
    return "module:#{match[1]}" if match
    match = snippet.match(/\b(struct|enum|trait|union)\s+([A-Za-z_][A-Za-z0-9_]*)/)
    return "type:#{match[2]}" if match
    match = snippet.match(/\b(const|static)\s+([A-Za-z_][A-Za-z0-9_]*)/)
    return "const:#{match[2]}" if match
    if (match = snippet.match(/\bimpl(?:<[^>]*>)?\s+(.+?)\s*\{/))
      return "impl:#{match[1].gsub(/\s+/, ' ').strip}"
    end
  end
  nil
end

def inline_metadata(line)
  match = line.match(INLINE)
  return nil unless match

  match[1].scan(/([a-z_]+)=([^\s]+)/).to_h
end

def scan(root)
  occurrences = Hash.new(0)
  errors = []
  records = source_files(root).flat_map do |file|
    relative = Pathname(file).relative_path_from(Pathname(root)).to_s
    lines = File.readlines(file, chomp: true)
    lines.each_index.map do |index|
      line = lines[index]
      next unless line.match?(ATTRIBUTE)

      symbol = symbol_after(lines, index, line.include?("#!["))
      unless symbol
        errors << "#{relative}:#{index + 1}: cannot determine the attributed symbol"
        next
      end
      key = [relative, symbol]
      occurrences[key] += 1
      {
        "path" => relative,
        "symbol" => symbol,
        "occurrence" => occurrences[key],
        "line" => index + 1,
        "inline" => inline_metadata(line)
      }
    end.compact
  end
  [records, errors]
end

def record_key(record)
  [record["path"], record["symbol"], record.fetch("occurrence", 1)]
end

def debt_owner(path)
  case path
  when %r{\Acrates/core/} then ["core", "#485"]
  when %r{\Acrates/daemon/}, %r{\Asrc/runtime/daemon\.rs\z} then ["daemon", "#486"]
  when %r{\Acrates/tui/}, %r{\Asrc/(runtime/tui|tui_input)\.rs\z} then ["tui", "#487"]
  else ["root-cli", "root-cli-follow-up"]
  end
end

def generated_manifest(records)
  entries = records.map do |record|
    owner, tracking = debt_owner(record["path"])
    {
      "path" => record["path"],
      "symbol" => record["symbol"],
      "occurrence" => record["occurrence"],
      "reason" => DEBT_REASON,
      "owner" => owner,
      "expires" => "2027-01-31",
      "tracking" => tracking
    }
  end
  {"version" => 1, "entries" => entries}
end

def validate_metadata(metadata, location, today, debt_allowed: false)
  errors = []
  required = %w[reason owner expires]
  required.each { |field| errors << "#{location}: missing #{field}" if metadata[field].to_s.empty? }
  reason = metadata["reason"]
  valid_reasons = debt_allowed ? ALLOWED_REASONS + [DEBT_REASON] : ALLOWED_REASONS
  errors << "#{location}: forbidden reason #{reason.inspect}" unless reason.nil? || valid_reasons.include?(reason)

  if ALLOWED_REASONS.include?(reason) && metadata["tests"].to_s.empty?
    errors << "#{location}: #{reason} requires fake/integration test evidence in tests"
  elsif reason == DEBT_REASON && metadata["tracking"].to_s.empty?
    errors << "#{location}: migration_debt requires tracking"
  end

  unless metadata["expires"].to_s.empty?
    begin
      expires = Date.iso8601(metadata["expires"])
      errors << "#{location}: expired on #{expires}" if expires < today
    rescue Date::Error
      errors << "#{location}: invalid expires #{metadata['expires'].inspect}"
    end
  end
  errors
end

def validate(options, records, scan_errors)
  errors = scan_errors.dup
  manifest_path = File.expand_path(options.manifest, options.root)
  manifest = JSON.parse(File.read(manifest_path))
  errors << "#{options.manifest}: version must be 1" unless manifest["version"] == 1
  entries = manifest["entries"]
  unless entries.is_a?(Array)
    return errors << "#{options.manifest}: entries must be an array"
  end

  by_key = {}
  entries.each_with_index do |entry, index|
    location = "#{options.manifest}:entries[#{index}]"
    %w[path symbol reason owner expires].each do |field|
      errors << "#{location}: missing #{field}" if entry[field].to_s.empty?
    end
    key = record_key(entry)
    if by_key.key?(key)
      errors << "#{location}: duplicate entry for #{key.join(':')}"
    else
      by_key[key] = entry
    end
    errors.concat(validate_metadata(entry, location, options.today, debt_allowed: true))
  end

  source_keys = records.map { |record| record_key(record) }
  records.each do |record|
    location = "#{record['path']}:#{record['line']}"
    if record["inline"]
      errors.concat(validate_metadata(record["inline"], location, options.today))
      errors << "#{location}: inline exception must not also be in the allowlist" if by_key.key?(record_key(record))
    elsif !by_key.key?(record_key(record))
      errors << "#{location}: unregistered coverage(off) for #{record['symbol']} occurrence #{record['occurrence']}"
    end
  end
  by_key.each_key do |key|
    errors << "#{options.manifest}: stale symbol #{key.join(':')}" unless source_keys.include?(key)
  end
  errors
rescue Errno::ENOENT => error
  ["coverage-off-lint: #{error.message}"]
rescue JSON::ParserError => error
  ["#{options.manifest}: invalid JSON: #{error.message}"]
end

options = parse_options
records, scan_errors = scan(File.expand_path(options.root))
if options.generate
  abort scan_errors.join("\n") unless scan_errors.empty?
  puts JSON.pretty_generate(generated_manifest(records))
  exit
end

errors = validate(options, records, scan_errors)
if errors.empty?
  puts "coverage-off-lint: ok (#{records.length} exclusions)"
else
  warn errors.map { |error| "ERROR: #{error}" }.join("\n")
  warn "coverage-off-lint: failed (#{errors.length} errors)"
  exit 1
end
