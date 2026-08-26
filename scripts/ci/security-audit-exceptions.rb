#!/usr/bin/env ruby
# frozen_string_literal: true

require "date"
require "json"

begin
  path = ARGV.fetch(0, ".github/security-audit-exceptions.json")
  today = Date.iso8601(ENV.fetch("SECURITY_AUDIT_TODAY", Date.today.iso8601))
  manifest = JSON.parse(File.read(path, encoding: Encoding::UTF_8))
  errors = []

  unless manifest.is_a?(Hash)
    warn "#{path}: manifest must be an object"
    exit 1
  end
  errors << "#{path}: version must be 1" unless manifest["version"] == 1
  unknown_manifest_fields = manifest.keys - %w[version exceptions]
  errors << "#{path}: unknown fields: #{unknown_manifest_fields.join(', ')}" unless unknown_manifest_fields.empty?
  exceptions = manifest["exceptions"]
  unless exceptions.is_a?(Array)
    warn "#{path}: exceptions must be an array"
    exit 1
  end

  advisories = []
  exceptions.each_with_index do |entry, index|
    location = "#{path}: exceptions[#{index}]"
    unless entry.is_a?(Hash)
      errors << "#{location}: must be an object"
      next
    end

    required = %w[advisory owner expires rationale]
    unknown_fields = entry.keys - required
    errors << "#{location}: unknown fields: #{unknown_fields.join(', ')}" unless unknown_fields.empty?
    required.each do |field|
      errors << "#{location}: missing #{field}" unless entry[field].is_a?(String) && !entry[field].strip.empty?
    end

    advisory = entry["advisory"].is_a?(String) ? entry["advisory"] : ""
    errors << "#{location}: invalid advisory #{advisory.inspect}" unless advisory.match?(/\ARUSTSEC-\d{4}-\d{4}\z/)
    owner = entry["owner"].is_a?(String) ? entry["owner"] : ""
    unless owner.match?(/\A@[A-Za-z0-9](?:[A-Za-z0-9-]{0,37}[A-Za-z0-9])?\z/)
      errors << "#{location}: owner must be a GitHub handle"
    end
    rationale = entry["rationale"].is_a?(String) ? entry["rationale"].strip : ""
    errors << "#{location}: rationale must be at least 20 characters" if rationale.length < 20

    begin
      expires = Date.iso8601(entry["expires"].to_s)
      errors << "#{location}: expired on #{expires}" if expires < today
      errors << "#{location}: expires more than 90 days from #{today}" if expires > today + 90
    rescue Date::Error
      errors << "#{location}: invalid expires #{entry['expires'].inspect}"
    end

    advisories << advisory unless advisory.empty?
  end

  duplicates = advisories.group_by { |advisory| advisory }.select { |_advisory, entries| entries.length > 1 }.keys
  errors << "#{path}: duplicate advisories: #{duplicates.join(', ')}" unless duplicates.empty?

  unless errors.empty?
    warn errors.join("\n")
    exit 1
  end

  puts "ignore=#{advisories.join(',')}"
  puts "validated #{advisories.length} RustSec exception(s)" if ENV["GITHUB_OUTPUT"].to_s.empty?
rescue ArgumentError, JSON::ParserError, KeyError => error
  warn "security-audit-exceptions: #{error.message}"
  exit 1
end
