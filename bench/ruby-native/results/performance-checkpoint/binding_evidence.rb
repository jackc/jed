# frozen_string_literal: true

require 'json'
require 'digest'
require_relative '../../binding_compare_lane'

module CheckpointBindingEvidence
  NATIVE = %w[native-prepared native-query native-eager].freeze
  STATES = %w[1-before 2-final 3-final 4-before].freeze
  module_function

  def median(values) = values.sort.fetch(values.length / 2)

  def paired(values)
    before, final = values.values_at(0, 3), values.values_at(1, 2)
    { before_us: before.sum / 2000.0, final_us: final.sum / 2000.0,
      speedup: before.sum.fdiv(final.sum), elapsed_change_percent: (final.sum.fdiv(before.sum) - 1) * 100,
      before_medians_ns: before, final_medians_ns: final,
      final_medians_below_both_before: final.max < before.min,
      final_medians_above_both_before: final.min > before.max }
  end

  def inspect_report(name, kind, source, runtime, lanes, verify: false)
    report = JSON.parse(File.read(File.join(@directory, "#{name}.json")))
    options = report.fetch('options')
    identity = @runtime_info.fetch(runtime)
    raise "failed binding report #{name}" unless report.fetch('errors').empty?
    raise 'binding runtime options differ' unless options.values_at('ruby', 'jit') == identity.values_at(:executable, :jit)
    raise 'binding verification option differs' unless !!options['verify_only'] == verify
    provenance = report.fetch('source_sha256')
    raise 'binding sources changed' unless provenance == report.fetch('source_sha256_after') && provenance.fetch('native') == source
    worker = File.join(@root, 'bench/ruby-native/binding_compare_lane.rb')
    raise 'binding worker changed' unless provenance.fetch('worker') == Digest::SHA256.file(worker).hexdigest
    wrapper_library = File.join(@root, 'impl/ruby/ext/target/release/libjed_ruby.so')
    raise 'binding native library changed' unless provenance.fetch('wrapper_library') == Digest::SHA256.file(wrapper_library).hexdigest
    path = @plan_paths.fetch(kind)
    plan = @plans.fetch(kind)
    raise 'binding selected plan differs' unless provenance['selected_plan'] == (kind == 'file' ? nil : Digest::SHA256.file(path).hexdigest)
    raise 'binding explicit plan differs' unless options['plan'] == (kind == 'file' ? nil : path)
    if kind == 'file'
      dataset = File.join(@root, '.dev/ruby-native-bench-data/large.jed')
      raise 'binding dataset changed' unless options['file'] == dataset && provenance['database'] == Digest::SHA256.file(dataset).hexdigest
    end
    raise 'binding lanes differ' unless report.fetch('lanes').keys.sort == lanes.sort
    report.fetch('lanes').each do |lane, entry|
      raise 'binding worker failed/bounds differ' unless entry['success'] && entry['reason'].nil? && entry.values_at('memory_limit_bytes', 'address_space_bytes') == [536870912, 2147483648]
      result = entry.fetch('result')
      raise 'binding runtime differs' unless result.fetch('runtime').start_with?(identity.fetch(:version)) && result.fetch('ruby_executable') == identity.fetch(:executable)
      raise 'binding runtime binary changed' unless result['ruby_executable_sha256'] == Digest::SHA256.file(identity.fetch(:executable)).hexdigest
      raise 'binding JIT differs' unless result.fetch('jit') == { 'yjit' => identity[:jit] == 'yjit', 'zjit' => identity[:jit] == 'zjit' }
      raise 'binding command JIT differs' unless entry.fetch('command').include?('--yjit') == (identity[:jit] == 'yjit') && entry.fetch('command').include?('--zjit') == (identity[:jit] == 'zjit')
      raise 'binding worker identity differs' unless result['engine'] == lane && result['script_sha256'] == provenance['worker'] && result['verify_only'] == verify
      raise 'binding source changed' unless result['source_sha256'] == result['final_source_sha256']
      raise 'binding selected source differs' unless lane == 'wrapper' || result['source_sha256'] == source
      raise 'binding library differs' if lane == 'wrapper' && result['native_library_sha256'] != provenance['wrapper_library']
      raise 'binding worker plan differs' unless result.values_at('plan_path', 'plan_sha256') == [path, Digest::SHA256.file(path).hexdigest]
      raise 'binding storage/fixture differs' unless result.values_at('storage', 'dataset_rows', 'repetitions') == [kind == 'file' ? 'file' : 'memory', kind == 'file' ? 1_000_000 : 1000, 5]
      raise 'binding compiler tier disabled' if %w[pipeline_disabled ordering_disabled aggregate_disabled relation_disabled text_compilation_disabled fresh_sql].any? { |flag| result[flag] }
      raise 'binding workloads differ' unless result.fetch('workloads').map { |job| job['name'] } == plan.fetch('workloads').map { |job| job['name'] }
      result.fetch('workloads').each do |job|
        spec = plan.fetch('workloads').find { |candidate| candidate['name'] == job['name'] }
        raise 'binding SQL/schedule differs' unless job.values_at('sql', 'warmup', 'iterations') == spec.values_at('sql', 'warmup', 'iterations')
        validation_count = spec.fetch('validation_iterations', plan.fetch('validation_iterations'))
        parameters = Array.new(validation_count) do |index|
          spec.key?('fixed_params') ? spec.fetch('fixed_params') : spec.fetch('span').zero? ? [] : [spec.fetch('offset') + (index * spec.fetch('step')) % spec.fetch('span')]
        end
        raise 'binding parameter cycle differs' unless job.fetch('validation_parameters') == parameters
        signature = job.slice('validation_sha256', 'answer_sha256', 'validation_rows', 'validation_costs', 'validation_columns', 'validation_column_types')
        key = "#{kind}/#{job.fetch('name')}"
        raise "binding values/metadata/costs differ #{key}" if @answers[key] && @answers[key] != signature
        @answers[key] = signature
        callbacks = job.fetch('callback_counts')
        callback_key = "#{key}/#{lane == 'wrapper' ? 'wrapper' : 'native'}"
        raise "callback batching changed #{callback_key}" if @callbacks[callback_key] && @callbacks[callback_key] != callbacks
        @callbacks[callback_key] = callbacks
        rows = job.fetch('validation_rows').sum
        if job['name'] == 'host_scalar' || job['name'].match?(/\Ahost_.*_row\z/)
          raise 'scalar callback count differs' unless callbacks == { 'row_calls' => rows, 'batch_calls' => 0, 'batch_sizes' => [] }
        elsif job['name'].match?(/\Ahost_.*_batch\z/)
          sizes = callbacks.fetch('batch_sizes')
          raise 'batch callback consumption differs' unless callbacks['row_calls'].zero? && callbacks['batch_calls'] == sizes.length && sizes.sum == rows && sizes.max > 1
        else
          raise 'unexpected callback' unless callbacks == { 'row_calls' => 0, 'batch_calls' => 0, 'batch_sizes' => [] }
        end
        if verify
          raise 'verification unexpectedly timed queries' unless job['samples_ns'].empty? && job['median_ns'].nil?
        else
          raise 'binding sample count differs' unless job['samples_ns'].length == 5 && job['rows_per_sample'].length == 5
          raise 'binding median differs' unless job['median_ns'] == median(job['samples_ns']).fdiv(job['iterations'])
          schedule = job.slice('iterations', 'warmup', 'rows_per_sample')
          raise "binding consumed row counts differ #{key}" if @schedules[key] && @schedules[key] != schedule
          @schedules[key] = schedule
        end
      end
      @peaks << entry.fetch('peak_rss_bytes')
    end
    report
  end

  def medians(report)
    report.fetch('lanes').transform_values do |lane|
      lane.fetch('result').fetch('workloads').to_h { |job| [job.fetch('name'), job.fetch('median_ns')] }
    end
  end

  def build(directory:, root:, baseline:, final_source:, runtime_info:)
    @directory, @root, @runtime_info = directory, root, runtime_info
    @answers, @callbacks, @schedules, @peaks = {}, {}, {}, []
    @plan_paths = { 'memory' => 'checkpoint-plan.json', 'host' => 'checkpoint-host-plan.json', 'file' => 'plan.json' }.transform_values { |name| File.join(root, 'bench/ruby-native/compare', name) }
    @plans = @plan_paths.transform_values { |path| JSON.parse(File.read(path)) }
    @plans['file'] = { 'validation_iterations' => 32, 'workloads' => RubyBindingComparison.file_workloads }
    validation = %w[before final].to_h do |state|
      source = state == 'before' ? baseline : final_source
      [state, %w[memory host].to_h do |kind|
        name = "verify-#{kind}-#{state}"
        inspect_report(name, kind, source, '4.0-yjit', state == 'before' ? ['wrapper', *NATIVE] : NATIVE, verify: true)
        [kind, "#{name}.json"]
      end]
    end
    controls = %w[memory host].to_h do |kind|
      reports = STATES.map do |state|
        inspect_report("#{kind}-4.0-yjit-#{state}", kind, state.end_with?('before') ? baseline : final_source, '4.0-yjit', NATIVE)
      end
      data = reports.map { |report| medians(report) }
      [kind, NATIVE.to_h do |lane|
        [lane, data.first.fetch(lane).keys.to_h { |name| [name, paired(data.map { |run| run.fetch(lane).fetch(name) })] }]
      end]
    end
    characterization = %w[4.0-none 4.1-zjit].to_h do |runtime|
      [runtime, %w[memory host].to_h do |kind|
        report = inspect_report("#{kind}-#{runtime}-final", kind, final_source, runtime, NATIVE)
        [kind, medians(report).transform_values { |jobs| jobs.transform_values { |value| value / 1000.0 } }]
      end]
    end
    binding = %w[memory host file].to_h do |kind|
      name = "binding-#{kind}-4.0-yjit"
      if kind == 'file'
        original = JSON.parse(File.read(File.join(@directory, "#{name}.json")))
        lanes = original.fetch('lanes')
        raise 'original file binding lanes differ' unless lanes.keys.sort == ['wrapper', *NATIVE].sort
        name += '-retry' unless lanes.values.all? { |entry| entry.fetch('success') }
      end
      report = inspect_report(name, kind, final_source, '4.0-yjit', ['wrapper', *NATIVE])
      data = medians(report)
      [kind, data.fetch('wrapper').to_h do |name, wrapper|
        [name, { wrapper_us: wrapper / 1000.0, native: NATIVE.to_h do |lane|
          value = data.fetch(lane).fetch(name)
          [lane, { us: value / 1000.0, native_over_wrapper: value.fdiv(wrapper) }]
        end }]
      end]
    end
    { statistic: 'YJIT source ABBA uses mean of two process medians; final interpreter/ZJIT and actual binding comparisons are fresh single-process observations. All measured queries consume their entire result.',
      controls: controls, final_characterization_us: characterization, actual_binding: binding,
      validation: validation, exact_values_metadata_costs: @answers, callback_schedules: @callbacks,
      callback_note: 'Buffered native callbacks use 64-row batches, whereas the Rust wrapper buffers all 500 rows in this fixture. Both return identical values and costs and consume every row; their batch-call counts are intentionally recorded separately.', schedules: @schedules,
      max_sampled_rss_bytes: @peaks.max }
  end
end
