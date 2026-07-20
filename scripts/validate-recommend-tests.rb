#!/usr/bin/env ruby
# frozen_string_literal: true

require "json"

root, map_file = ARGV
abort "usage: validate-recommend-tests.rb ROOT MAP METADATA_JSON" unless ARGV.length == 3

metadata = JSON.parse(File.read(ARGV.fetch(2)))
packages = metadata.fetch("packages").to_h { |package| [package.fetch("name"), package] }
root_manifest = File.join(root, "Cargo.toml")
root_package = packages.values.find { |package| package.fetch("manifest_path") == root_manifest }
errors = []
patterns = []

def expand(command, witness)
  test = File.basename(witness, ".rs")
  command.gsub("{test}", test)
end

File.foreach(map_file).with_index(1) do |line, line_number|
  next if line.strip.empty? || line.start_with?("#")

  pattern, area, reason, templates, witness, extra = line.chomp.split("\t", -1)
  if [pattern, area, reason, templates, witness].any? { |field| field.nil? || field.empty? } || extra
    errors << "#{map_file}:#{line_number}: expected five non-empty tab-separated fields"
    next
  end

  unless File.fnmatch?(pattern, witness)
    errors << "#{map_file}:#{line_number}: witness #{witness.inspect} does not match #{pattern.inspect}"
  end
  shadow = patterns.find { |earlier, _| File.fnmatch?(earlier, witness) }
  if shadow
    errors << "#{map_file}:#{line_number}: rule #{pattern.inspect} is shadowed for its witness by line #{shadow.last} (#{shadow.first.inspect})"
  end
  patterns << [pattern, line_number]

  unless File.exist?(File.join(root, witness))
    errors << "#{map_file}:#{line_number}: witness path does not exist: #{witness}"
  end

  templates.split("|").each do |template|
    command = expand(template, witness)
    next unless command.start_with?("cargo test ")
    next if command.include?("--manifest-path") || command.include?("--workspace")

    package_name = command[/\s-p\s+(\S+)/, 1]
    package = package_name ? packages[package_name] : root_package
    unless package
      errors << "#{map_file}:#{line_number}: command names missing package: #{package_name}"
      next
    end

    target_kind = command[/\s--(test|bin)\s+(\S+)/, 1]
    target_name = command[/\s--(?:test|bin)\s+(\S+)/, 1]
    next unless target_name

    targets = package.fetch("targets").select { |target| target.fetch("kind").include?(target_kind) }.map { |target| target.fetch("name") }
    unless targets.include?(target_name)
      errors << "#{map_file}:#{line_number}: package #{package.fetch('name')} has no #{target_kind} target #{target_name}"
    end
  end
end

if errors.empty?
  puts "recommend-tests map: ok"
else
  warn errors.join("\n")
  exit 1
end
