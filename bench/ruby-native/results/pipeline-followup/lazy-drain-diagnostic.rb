# frozen_string_literal: true

# Diagnostic only. The ordinary corpus runner calls Rows#rows, an engine-owned
# collector with its own memory allowance. Substitute host-owned .each
# collection to locate failures caused by that API choice. This does not alter
# the full lazy audit or establish that lazy execution passes the corpus.
require_relative '../../../../impl/ruby-native/bin/conformance'

module PipelineExternalDrainDiagnostic
  def rows
    @pipeline_diagnostic_host_rows ||= each.to_a
  end
end

JedNative::Rows.prepend(PipelineExternalDrainDiagnostic)

path = ENV.fetch('JED_NATIVE_REPORT')
passed = NativeCorpus.new(mode: 'memory', output: path).run
report = JSON.parse(File.read(path))
report['diagnostic'] = {
  'collector' => 'Host-owned Rows#each.to_a; no Rows#rows collector reservation',
  'mechanism' => 'Prepend a diagnostic-only Rows#rows method that caches each.to_a without setting @collecting',
  'scope' => 'The same affected files selected by the complete lazy audit',
  'official_conformance_result' => false,
  'driver_sha256' => Digest::SHA256.file(__FILE__).hexdigest
}
File.write(path, JSON.pretty_generate(report) + "\n")
exit(passed ? 0 : 1)
