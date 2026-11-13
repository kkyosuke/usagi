#!/usr/bin/env ruby
# frozen_string_literal: true

# Enforce immutable third-party Actions and a read-only workflow baseline.

require "yaml"

def check_action_pin(action, relative, failures)
  unless action.is_a?(String) && !action.strip.empty?
    failures << "#{relative} has an invalid uses declaration: #{action.inspect}"
    return
  end
  return if action.start_with?("./")

  reference = action.split("@", 2)[1]
  return if reference&.match?(/\A[0-9a-f]{40}\z/)

  failures << "#{relative} must pin #{action} to a full commit SHA"
end

def workflow_permissions_read_only?(permissions)
  case permissions
  when "read-all"
    true
  when Hash
    permissions.values.all? { |level| %w[read none].include?(level) }
  else
    false
  end
end

root = File.expand_path(ARGV.fetch(0, File.join(__dir__, "../..")))
failures = []
workflows = Dir.glob(File.join(root, ".github/workflows/*.{yml,yaml}")).sort

workflows.each do |path|
  relative = path.delete_prefix("#{root}/")
  source = File.read(path, encoding: Encoding::UTF_8)
  begin
    document = Psych.safe_load(source, permitted_classes: [], aliases: false)
  rescue Psych::Exception => e
    failures << "#{relative} is not valid YAML: #{e.message}"
    next
  end

  unless document.is_a?(Hash)
    failures << "#{relative} must parse as a YAML mapping"
    next
  end

  if !document.key?("permissions")
    failures << "#{relative} is missing top-level permissions"
  elsif !workflow_permissions_read_only?(document["permissions"])
    failures << "#{relative} grants write permission at workflow scope"
  end

  jobs = document["jobs"]
  next unless jobs.is_a?(Hash)

  jobs.each_value do |job|
    next unless job.is_a?(Hash)

    check_action_pin(job["uses"], relative, failures) if job.key?("uses")
    steps = job["steps"]
    next unless steps.is_a?(Array)

    steps.each do |step|
      next unless step.is_a?(Hash) && step.key?("uses")

      check_action_pin(step["uses"], relative, failures)
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
