#!/usr/bin/env ruby
# frozen_string_literal: true

# lcov.info を読み、カバレッジ未達 (lines/functions < 閾値) のファイルと
# 未達関数・未達行を Markdown で出力する。coverage.yml がこの出力を PR コメントと
# Job Summary (GITHUB_STEP_SUMMARY) の両方へ流す。生成ロジックをここへ抽出する
# ことで、fixture ベースの test (scripts/tests/coverage-report-comment.sh) で
# 固定でき、YAML 内の inline スクリプトでは不可能なユニット検証を行える。
#
# 使い方:
#   ruby scripts/coverage-report-comment.rb [lcov.info]
#
# 環境変数 (すべて任意):
#   COVERAGE_MIN         閾値 (%)。既定 100。scripts/coverage.sh が export する。
#   MAX_FILES            表示する未達ファイルの上限。既定 20。
#   MAX_FUNCS_PER_FILE   1 ファイルあたり表示する未達関数の上限。既定 10。
#   MAX_LINE_RANGES      1 ファイルあたり表示する未達行レンジの上限。既定 20。
#   GITHUB_WORKSPACE     ファイル path から取り除くリポジトリルート。既定は cwd。
#
# cargo-llvm-cov の `--lcov` 出力は既定で関数レコード (FN/FNDA/FNF/FNH) を含み、
# 関数名は demangle 済み。ここでは lcov のみを入力とし、cargo の再実行はしない。

lcov_path = ARGV[0] || "lcov.info"
threshold = (ENV["COVERAGE_MIN"] || "100").to_f
max_files = (ENV["MAX_FILES"] || "20").to_i
max_funcs = (ENV["MAX_FUNCS_PER_FILE"] || "10").to_i
max_ranges = (ENV["MAX_LINE_RANGES"] || "20").to_i

abort "coverage-report-comment: #{lcov_path} が見つかりません" unless File.file?(lcov_path)

root = (ENV["GITHUB_WORKSPACE"] || Dir.pwd).sub(%r{/*\z}, "") + "/"

FileCov = Struct.new(:path, :fn_lines, :fn_hits, :uncovered_lines, :lf, :lh,
                     keyword_init: true)

records = []
current = nil
File.foreach(lcov_path) do |raw|
  line = raw.chomp
  case line
  when /\ASF:(.*)/
    current = FileCov.new(path: Regexp.last_match(1), fn_lines: {}, fn_hits: {},
                          uncovered_lines: [], lf: 0, lh: 0)
  when /\AFN:(\d+),(.*)/
    current.fn_lines[Regexp.last_match(2)] = Regexp.last_match(1).to_i
  when /\AFNDA:(\d+),(.*)/
    current.fn_hits[Regexp.last_match(2)] = Regexp.last_match(1).to_i
  when /\ADA:(\d+),(\d+)/
    current.lf += 1
    if Regexp.last_match(2).to_i.zero?
      current.uncovered_lines << Regexp.last_match(1).to_i
    else
      current.lh += 1
    end
  when "end_of_record"
    records << current if current
    current = nil
  end
end

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

Row = Struct.new(:path, :fnf, :fnh, :lf, :lh, :uncovered_fns, :fn_lines,
                 :uncovered_lines, keyword_init: true)

rows = records.map do |r|
  names = (r.fn_lines.keys | r.fn_hits.keys)
  fnh = names.count { |n| (r.fn_hits[n] || 0).positive? }
  uncovered = names.reject { |n| (r.fn_hits[n] || 0).positive? }
                   .sort_by { |n| [r.fn_lines[n] || 0, n] }
  Row.new(path: r.path.start_with?(root) ? r.path[root.length..] : r.path,
          fnf: names.length, fnh: fnh, lf: r.lf, lh: r.lh,
          uncovered_fns: uncovered, fn_lines: r.fn_lines,
          uncovered_lines: r.uncovered_lines)
end

tot_lf = rows.sum(&:lf)
tot_lh = rows.sum(&:lh)
tot_fnf = rows.sum(&:fnf)
tot_fnh = rows.sum(&:fnh)
line_pct = rate(tot_lh, tot_lf)
fn_pct = rate(tot_fnh, tot_fnf)

incomplete = rows.select do |row|
  rate(row.lh, row.lf) < threshold || rate(row.fnh, row.fnf) < threshold
end.sort_by { |row| [rate(row.lh, row.lf), rate(row.fnh, row.fnf), row.path] }

out = []
out << "## 📊 Test Coverage"
out << ""

verdict = line_pct >= threshold && fn_pct >= threshold ? "✅ PASS" : "❌ FAIL"
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

  shown = incomplete.first(max_files)
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
      row.uncovered_fns.first(max_funcs).each do |name|
        line = row.fn_lines[name]
        loc = line ? " (L#{line})" : ""
        out << "  - #{code(name)}#{loc}"
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
