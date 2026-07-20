#!/usr/bin/env ruby
# frozen_string_literal: true

# cargo-llvm-cov の JSON レポート (`cargo llvm-cov report --json`) を読み、カバレッジ
# 未達 (lines/functions < 閾値) のファイルと未達関数・未達行を Markdown で出力する。
# coverage.yml がこの出力を PR コメントと Job Summary の両方へ流す。
#
# JSON を入力にするのは、gate (`cargo llvm-cov report --fail-under-*`) が使う集計と
# 完全に一致させるため。lcov 出力の関数レコード (FN/FNDA) は generic の単相化ごとに
# 1 件並ぶため、そのまま数えると gate と大きく食い違う（例: 実質 100% でも 76% に見える）。
# JSON の summary は単相化をマージした関数カバレッジで、gate と一致する。
#
# 使い方:
#   ruby scripts/coverage-report-comment.rb [coverage.json]
#
# 環境変数 (すべて任意):
#   COVERAGE_MIN         閾値 (%)。既定 100。scripts/coverage.sh が export する。
#   MAX_FILES            表示する未達ファイルの上限。既定 20。
#   MAX_FUNCS_PER_FILE   1 ファイルあたり表示する未達関数の上限。既定 10。
#   MAX_LINE_RANGES      1 ファイルあたり表示する未達行レンジの上限。既定 20。
#   DEMANGLER            Rust/C++ シンボルの demangler コマンド。既定 c++filt
#                        （binutils。Rust v0 を demangle する）。空にすると素の
#                        mangled 名のまま出力する。
#   GITHUB_WORKSPACE     ファイル path から取り除くリポジトリルート。既定は cwd。

require "json"

json_path = ARGV[0] || "coverage.json"
threshold = (ENV["COVERAGE_MIN"] || "100").to_f
max_files = (ENV["MAX_FILES"] || "20").to_i
max_funcs = (ENV["MAX_FUNCS_PER_FILE"] || "10").to_i
max_ranges = (ENV["MAX_LINE_RANGES"] || "20").to_i
demangler = ENV.fetch("DEMANGLER", "c++filt")

abort "coverage-report-comment: #{json_path} が見つかりません" unless File.file?(json_path)

data = JSON.parse(File.read(json_path)).fetch("data").fetch(0)

root = (ENV["GITHUB_WORKSPACE"] || Dir.pwd).sub(%r{/*\z}, "") + "/"
def rel(path, root)
  path.start_with?(root) ? path[root.length..] : path
end

# functions[] を (ファイル, 宣言行) で束ねる。generic の単相化は同じソース宣言行を
# 共有するため、宣言行で束ねると JSON summary のマージ済み関数数と一致する。
# ある関数はどれか 1 つの単相化でも実行されていれば covered。
fn_groups = Hash.new { |h, k| h[k] = [] }
data.fetch("functions", []).each do |fn|
  regions = fn["regions"]
  next if regions.nil? || regions.empty?

  file = fn["filenames"][0]
  fn_groups[file] << { decl: regions[0][0], count: fn["count"], name: fn["name"] }
end

def uncovered_fns_for(entries)
  entries.group_by { |e| e[:decl] }
         .select { |_, insts| insts.all? { |i| i[:count].zero? } }
         .map { |decl, insts| { decl: decl, name: insts[0][:name] } }
         .sort_by { |f| f[:decl] }
end

# regions[] から未達行を復元する（gap region は除外）。各行に被さる region の最大
# 実行回数を取り、0 の行が未達。これは lcov の DA:<line>,0 と一致する。
def uncovered_lines_for(entries_regions)
  line_max = {}
  entries_regions.each do |r|
    l1, _, l2, _, cnt, _, _, kind = r
    next if kind == 2 # gap region

    (l1..l2).each { |ln| line_max[ln] = [line_max[ln] || 0, cnt].max }
  end
  line_max.select { |_, c| c.zero? }.keys.sort
end

regions_by_file = Hash.new { |h, k| h[k] = [] }
data.fetch("functions", []).each do |fn|
  file = fn["filenames"][0]
  (fn["regions"] || []).each { |r| regions_by_file[file] << r }
end

Row = Struct.new(:path, :fnf, :fnh, :lf, :lh, :uncovered_fns, :uncovered_lines,
                 keyword_init: true)

rows = data.fetch("files").map do |f|
  fs = f["summary"]["functions"]
  ls = f["summary"]["lines"]
  file = f["filename"]
  Row.new(path: rel(file, root),
          fnf: fs["count"], fnh: fs["covered"], lf: ls["count"], lh: ls["covered"],
          uncovered_fns: uncovered_fns_for(fn_groups[file]),
          uncovered_lines: uncovered_lines_for(regions_by_file[file]))
end

totals = data.fetch("totals")
tot_lf = totals["lines"]["count"]
tot_lh = totals["lines"]["covered"]
tot_fnf = totals["functions"]["count"]
tot_fnh = totals["functions"]["covered"]

def rate(hit, found)
  return 100.0 if found.zero?

  100.0 * hit / found
end

def emoji(pct)
  if pct >= 90 then "🟢"
  elsif pct >= 70 then "🟡"
  else "🔴"
  end
end

# Markdown のコードスパン/テーブルセルを壊さないようにエスケープする。
def code(str)
  "`#{str.gsub('`', "'").gsub('|', '\\|')}`"
end

def compress(nums, limit)
  nums = nums.sort.uniq
  ranges = []
  nums.each do |n|
    if !ranges.empty? && ranges.last[1] == n - 1
      ranges.last[1] = n
    else
      ranges << [n, n]
    end
  end
  labels = ranges.map { |a, b| a == b ? "L#{a}" : "L#{a}-#{b}" }
  return labels, 0 if labels.length <= limit

  [labels.first(limit), labels.length - limit]
end

# 表示する mangled 名をまとめて demangle する（1 プロセスで一括処理）。
def demangle(names, demangler)
  return names if names.empty? || demangler.nil? || demangler.empty?

  begin
    out = IO.popen([demangler], "r+") do |io|
      io.write(names.join("\n") + "\n")
      io.close_write
      io.read
    end
    result = out.split("\n")
    result.length == names.length ? result : names
  rescue StandardError
    names
  end
end

incomplete = rows.select { |row| row.fnh < row.fnf || row.lh < row.lf }
                 .sort_by { |row| [rate(row.lh, row.lf), rate(row.fnh, row.fnf), row.path] }
shown = incomplete.first(max_files)

# 表示するファイルの未達関数名だけ demangle する。
name_map = {}
raw_names = shown.flat_map { |row| row.uncovered_fns.first(max_funcs).map { |f| f[:name] } }.uniq
demangle(raw_names, demangler).each_with_index { |dn, i| name_map[raw_names[i]] = dn }

line_pct = rate(tot_lh, tot_lf)
fn_pct = rate(tot_fnh, tot_fnf)

out = []
out << "## 📊 Test Coverage"
out << ""
verdict = tot_lh >= tot_lf && tot_fnh >= tot_fnf ? "✅ PASS" : "❌ FAIL"
out << format("> 🚀 **いまのカバレッジ — Lines: %.2f%% / Functions: %.2f%%**" \
              "（閾値 %g%%: %s）", line_pct, fn_pct, threshold, verdict)
out << ""

if incomplete.empty?
  out << "🎉✨ パーフェクト！全ファイル Lines/Functions カバレッジ 100% を達成しました 🏆🐰"
else
  out << "100% に届いていないファイルをピックアップしました 👇" \
         "（上限: ファイル #{max_files} 件 / 関数 #{max_funcs} 件）"
  out << ""
  out << "| 📄 ファイル | 🔧 Functions | 📈 Lines |"
  out << "| :--- | ---: | ---: |"
  shown.each do |row|
    fpct = rate(row.fnh, row.fnf)
    lpct = rate(row.lh, row.lf)
    fcell = format("%.2f%% (不足 %d)", fpct, row.fnf - row.fnh)
    lcell = format("%s %.2f%% (不足 %d)", emoji(lpct), lpct, row.lf - row.lh)
    out << "| #{code(row.path)} | #{fcell} | #{lcell} |"
  end
  out << format("| **🧮 合計** | **%.2f%% (不足 %d)** | **%.2f%% (不足 %d)** |",
                fn_pct, tot_fnf - tot_fnh, line_pct, tot_lf - tot_lh)
  if incomplete.length > shown.length
    out << ""
    out << "> …ほか #{incomplete.length - shown.length} ファイル"
  end

  shown.each do |row|
    out << ""
    out << "#### #{code(row.path)}"
    unless row.uncovered_fns.empty?
      out << ""
      out << "- 🔧 未達関数 (#{row.uncovered_fns.length}):"
      row.uncovered_fns.first(max_funcs).each do |f|
        out << "  - #{code(name_map.fetch(f[:name], f[:name]))} (L#{f[:decl]})"
      end
      extra = row.uncovered_fns.length - max_funcs
      out << "  - …ほか #{extra} 関数" if extra.positive?
    end
    next if row.uncovered_lines.empty?

    labels, hidden = compress(row.uncovered_lines, max_ranges)
    suffix = hidden.positive? ? " …ほか #{hidden} 箇所" : ""
    out << "" if row.uncovered_fns.empty?
    out << "- 📈 未達行 (#{row.lf - row.lh}): #{labels.join(', ')}#{suffix}"
  end
end

puts out.join("\n")
