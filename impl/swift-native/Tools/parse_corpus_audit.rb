#!/usr/bin/env ruby
# frozen_string_literal: true

require_relative "generate_spec"

# A standalone compiler invocation avoids touching SwiftPM while another agent
# owns its integrated suite. Only the handwritten parser and its data compile.
root = SwiftSpecGenerator::ROOT
baseline = SwiftSpecGenerator::BASELINE
package = File.join(root, "impl/swift-native")
build = File.join(package, ".build/parser-audit")
FileUtils.mkdir_p(File.join(build, "module-cache"))
sources = %w[Error Text Lexer GeneratedSpec SQLSyntax Parser ParserExpressions ParserStatements].map do |name|
  File.join(package, "Sources/Jed/#{name}.swift")
end
executable = File.join(build, "parser-audit")
abort "parser audit compilation failed" unless system(
  "swiftc", "-module-cache-path", File.join(build, "module-cache"), *sources,
  File.join(__dir__, "ParserAudit.swift"), "-o", executable, chdir: package
)

listing, status = Open3.capture2("git", "ls-tree", "-r", "--name-only", baseline,
                                "spec/conformance/suites", chdir: root)
abort "cannot list frozen corpus" unless status.success?
paths = listing.lines.map(&:chomp).select { |path| path.end_with?(".test") }.sort
abort "frozen corpus is empty" if paths.empty?
records = []
Open3.popen2("git", "cat-file", "--batch", chdir: root) do |input, output, process|
  input.binmode
  output.binmode
  paths.each do |path|
    input.puts "#{baseline}:#{path}"
    input.flush
    header = output.gets&.split
    abort "cannot read frozen corpus #{path}" unless header&.length == 3 && header[1] == "blob"
    bytes = output.read(Integer(header[2]))
    abort "truncated frozen corpus #{path}" unless bytes&.bytesize == Integer(header[2]) && output.read(1) == "\n"
    lines = bytes.force_encoding(Encoding::UTF_8).lines
    cursor = 0
    while cursor < lines.length
      header = lines[cursor].strip
      if header.start_with?("statement ", "query ")
        source_line = cursor + 1
        cursor += 1
        sql = []
        while cursor < lines.length && !lines[cursor].strip.empty? && lines[cursor].strip != "----"
          sql << lines[cursor]
          cursor += 1
        end
        fields = header.split
        records << {file: path, line: source_line,
                    expected: fields[1] == "error" ? fields.fetch(2) : "ok", sql: sql.join.strip}
      end
      cursor += 1
    end
  end
  input.close
  abort "cannot read frozen corpus" unless process.value.success?
end
warn "Frozen baseline #{baseline}: #{paths.length} corpus files, #{records.length} SQL records; parsing only."
payload = records.map { |record| JSON.generate(record) }.join("\n") + "\n"
stdout, stderr, status = Open3.capture3(executable, stdin_data: payload, chdir: package)
$stdout.write(stdout)
$stderr.write(stderr)
exit(status.exitstatus || 1)
