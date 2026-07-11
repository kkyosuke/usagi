#!/usr/bin/env ruby
# frozen_string_literal: true

require "rexml/document"

abort "usage: #{File.basename($PROGRAM_NAME)} JUNIT..." if ARGV.empty?

runs = ARGV.map do |path|
  tests = {}
  REXML::Document.new(File.read(path)).elements.each("//testcase") do |testcase|
    name = [testcase.attributes["classname"], testcase.attributes["name"]]
           .compact.map(&:to_s).join("::")
    tests[name] = testcase.attributes.fetch("time").to_s.to_f
  end
  tests
end

names = runs.flat_map(&:keys).uniq
rows = names.map do |name|
  samples = runs.map { |run| run[name] }.compact
  mean = samples.sum / samples.length
  [name, mean, samples.min, samples.max, samples.max - samples.min, samples.length]
end

puts "# nextest duration summary"
puts
puts "Runs: #{runs.length}; tests observed: #{names.length}; retries: disabled"
puts
puts "| Test | Mean (s) | Min (s) | Max (s) | Range (s) | Samples |"
puts "|---|---:|---:|---:|---:|---:|"
rows.sort_by { |row| -row[1] }.first(20).each do |name, mean, min, max, range, count|
  puts "| `#{name.gsub("|", "\\|")}` | #{format('%.3f', mean)} | #{format('%.3f', min)} | #{format('%.3f', max)} | #{format('%.3f', range)} | #{count} |"
end
