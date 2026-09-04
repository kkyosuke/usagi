#!/usr/bin/env ruby
# frozen_string_literal: true

# Enforce immutable third-party Actions and a read-only workflow baseline.

root = File.expand_path(ARGV.fetch(0, File.join(__dir__, "../..")))
failures = []
workflows = Dir.glob(File.join(root, ".github/workflows/*.{yml,yaml}")).sort

workflows.each do |path|
  relative = path.delete_prefix("#{root}/")
  source = File.read(path, encoding: Encoding::UTF_8)

  failures << "#{relative} is missing top-level permissions" unless source.match?(/^permissions:\s*$/)
  source.lines.each_with_index do |line, index|
    if (match = line.match(/^\s*-?\s*uses:\s*([^\s#]+)/))
      action = match[1]
      next if action.start_with?("./")

      reference = action.split("@", 2)[1]
      unless reference&.match?(/\A[0-9a-f]{40}\z/)
        failures << "#{relative}:#{index + 1} must pin #{action} to a full commit SHA"
      end
    end
    if line.match?(/^  [a-z-]+:\s*write\s*$/)
      failures << "#{relative}:#{index + 1} grants write permission at workflow scope"
    end
  end
end

failures << "no GitHub Actions workflows found" if workflows.empty?
if failures.empty?
  puts "github-actions-policy: ok (#{workflows.length} workflows)"
  exit 0
end

warn "github-actions-policy: #{failures.length} problem(s)"
failures.each { |failure| warn "- #{failure}" }
exit 1
