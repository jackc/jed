# frozen_string_literal: true

# Baseline-only, untimed feasibility probe. No engine methods are overridden.
# Snapshot the frontier before an entire extension batch is generated, so
# candidates accepted earlier in that batch cannot inflate the opportunity.
require 'json'
require 'objspace'
require 'optparse'
require 'rbconfig'
require 'time'
require_relative '../../source'
require_relative '../../join_functions_diagnostics'

module NWayFrontierOpportunity
  ROOT = File.expand_path('../../../..', __dir__)
  BASELINE = '66afc525fcc2f4d0290ef70d03cc2f95e7521bd467029ecc653fe7b418f55341'
  PLAN = File.join(ROOT, 'bench/ruby-native/compare/join-functions-plan.json')
  DIMENSIONS = %i[cost rows logical].freeze
  module_function

  def dimensions(state) = DIMENSIONS.map { |key| state.fetch(key) }

  def dominate?(prior, candidate)
    prior.zip(candidate).all? { |left, right| left <= right } &&
      prior.zip(candidate).any? { |left, right| left < right }
  end

  def outcome(session, statement, params)
    result = session.execute_prepared(statement, *params)
    rows = []
    result.each { |row| rows << JoinFunctionsDiagnostics.image(row.to_a) }
    { rows: rows, columns: result.columns, types: result.column_types, cost: result.cost }
  ensure
    result&.close
  end

  def observe(session, statement, params)
    islands, extensions, batches, plans = [], [], [], []
    retain = Hash.new(0)
    trace = TracePoint.new(:call, :return) do |event|
      owner = event.defined_class.to_s
      if owner == 'JedNative::NWayJoins'
        case [event.event, event.method_id]
        when [:call, :nway_island]
          islands << event.binding
        when [:return, :nway_island]
          islands.pop
        when [:call, :nway_extensions]
          binding = event.binding
          island = islands.last
          frontier = island && island.local_variable_get(:frontier)
          snapshot = (frontier || {}).map do |bucket, states|
            { bucket: bucket.dup, dimensions: states.map { |state| dimensions(state) } }
          end
          extensions << { ordinal: binding.local_variable_get(:ordinal),
            fixed: binding.local_variable_get(:fixed), frontier_available: !frontier.nil?,
            frontier_before_batch: snapshot }
        when [:return, :nway_extensions]
          batch = extensions.pop
          batch[:candidates] = event.return_value.map do |candidate|
            bucket = [candidate.fetch(:mask), candidate.fetch(:ordered) ? 1 : 0]
            values = dimensions(candidate)
            prior = batch[:frontier_before_batch].find { |entry| entry[:bucket] == bucket }
            entries = prior ? prior[:dimensions] : []
            { bucket: bucket, dimensions: values,
              strictly_dominated_before_batch: entries.any? { |entry| dominate?(entry, values) },
              exact_dimension_tie_before_batch: entries.include?(values),
              retained_vector_lengths: %i[order accesses steps].map { |key| candidate.fetch(key).length } }
          end
          batches << batch
        when [:call, :nway_retain]
          candidate = event.binding.local_variable_get(:candidate)
          frontier = event.binding.local_variable_get(:frontier)
          bucket = [candidate.fetch(:mask), candidate.fetch(:ordered) ? 1 : 0]
          entries = (frontier[bucket] || []).map { |state| dimensions(state) }
          values = dimensions(candidate)
          retain[:calls] += 1
          retain[:strict_at_retain] += 1 if entries.any? { |entry| dominate?(entry, values) }
          retain[:exact_dimension_tie_at_retain] += 1 if entries.include?(values)
        end
      elsif owner == 'JedNative::PreparedJoins' && event.event == :return &&
          event.method_id == :nway_join_plan
        plans << JoinFunctionsDiagnostics.plan_image(event.return_value)
      end
    end
    GC.start
    ObjectSpace.trace_object_allocations_clear
    previously_disabled = GC.disable
    ObjectSpace.trace_object_allocations_start
    actual = trace.enable { outcome(session, statement, params) }
    ObjectSpace.trace_object_allocations_stop
    allocations = Hash.new(0)
    ObjectSpace.each_object do |object|
      next unless ObjectSpace.allocation_sourcefile(object) == @nway_file &&
        ObjectSpace.allocation_sourceline(object) == @candidate_line
      allocations[object.class.name] += 1
    end
    candidates = batches.flat_map { |batch| batch[:candidates] }
    eligible = batches.select { |batch| batch[:frontier_available] && !batch[:fixed] }
      .flat_map { |batch| batch[:candidates] }
    strict = eligible.count { |candidate| candidate[:strictly_dominated_before_batch] }
    # Each old construction allocates three [element] literals, three + results,
    # and its candidate Hash. Measure that site independently of retain counts.
    raise 'candidate allocation site did not match all returned candidates' unless
      allocations['Array'] == candidates.length * 6 && allocations['Hash'] == candidates.length
    { outcome: actual, physical_plans: plans.uniq, retain: retain,
      extension_batches: batches, constructed_candidates: candidates.length,
      frontier_candidates: eligible.length, strict_opportunities: strict,
      exact_tie_candidates: eligible.count { |candidate| candidate[:exact_dimension_tie_before_batch] },
      measured_candidate_site_allocations: allocations,
      avoidable_candidate_site_allocations: { 'Array' => strict * 6, 'Hash' => strict } }
  ensure
    trace&.disable
    ObjectSpace.trace_object_allocations_stop
    ObjectSpace.trace_object_allocations_clear
    GC.enable unless previously_disabled
  end

  def main
    sessions = nil
    options = {}
    OptionParser.new { |cli| cli.on('--report PATH') { |value| options[:report] = File.expand_path(value) } }.parse!
    abort 'supply a fresh --report path' unless ARGV.empty? && options[:report] && !File.exist?(options[:report])
    source = File.expand_path(ENV.fetch('RUBY_NATIVE_SOURCE'))
    before = NativeRubySource.digest(source)
    raise 'wrong baseline source' unless before == BASELINE
    require File.join(source, 'lib/jed_native')
    @nway_file = File.join(source, 'lib/jed_native/nway_joins.rb')
    lines = File.readlines(@nway_file)
    @candidate_line = lines.find_index { |line| line.include?('{ order: state[:order] + [ordinal]') }&.+(1)
    raise 'candidate construction site missing' unless @candidate_line
    plan = JSON.parse(File.read(PLAN))
    database = JedNative.memory
    setup = database.session
    setup.execute('CREATE TABLE items (id i32 PRIMARY KEY, bucket i32, score i32, label text)')
    values = plan.fetch('rows').times.map { |i| "(#{i},#{i % 10},#{(i * 37) % 10000},'row#{i}')" }.join(',')
    setup.execute("INSERT INTO items VALUES #{values}")
    setup.close
    jobs = plan.fetch('workloads') + [{ 'name' => 'five_table_bounded', 'offset' => 3, 'step' => 1, 'span' => 4,
      'sql' => 'SELECT a.id,b.score,c.label,d.bucket,e.id FROM items a JOIN items b ON b.id=a.id JOIN items c ON c.id=b.id JOIN items d ON d.id=c.id JOIN items e ON e.id=d.id WHERE a.id<$1 AND b.id<$1 AND c.id<$1 AND d.id<$1 AND e.id<$1 ORDER BY a.id' }]
    records = jobs.map do |job|
      sessions = [database.session, database.session]
      statements = sessions.map { |session| session.prepare(job.fetch('sql')) }
      params = 3.times.map { |i| job.fetch('span').zero? ? [] : [job.fetch('offset') + (i * job.fetch('step')) % job.fetch('span')] }
      cases = %w[cold warm changed].each_with_index.map do |state, index|
        selected = params[index == 1 ? 0 : index]
        if state == 'warm'
          3.times { sessions.each_with_index { |session, i| outcome(session, statements[i], params[0]) } }
        end
        record = observe(sessions.first, statements.first, selected)
        expected = outcome(sessions.last, statements.last, selected)
        raise "instrumentation changed #{job['name']} #{state}" unless record[:outcome] == expected
        record.merge(state: state, params: selected)
      end
      sessions.each(&:close)
      { name: job.fetch('name'), sql: job.fetch('sql'), cases: cases }
    end
    database.close
    after = NativeRubySource.digest(source)
    raise 'source changed' unless before == after
    report = { schema: 1, mode: 'untimed_baseline_frontier_opportunity', created_at: Time.now.utc.iso8601,
      source: source, source_sha256: before, final_source_sha256: after,
      script_sha256: Digest::SHA256.file(__FILE__).hexdigest,
      dependency_sha256: Digest::SHA256.file(File.join(ROOT, 'bench/ruby-native/join_functions_diagnostics.rb')).hexdigest,
      plan_sha256: Digest::SHA256.file(PLAN).hexdigest, rows: plan.fetch('rows'),
      ruby: RUBY_DESCRIPTION, ruby_executable: RbConfig.ruby, yjit: RubyVM::YJIT.enabled?,
      candidate_site: { path: 'lib/jed_native/nway_joins.rb', line: @candidate_line }, workloads: records,
      notes: ['No timing samples; original production methods are unchanged.',
        'Strict dominance uses only states already present before each entire extension batch; within-batch retention is excluded.',
        'Equal cost/rows/logical vectors are counted separately and remain subject to original deterministic tie breaking.',
        'ObjectSpace independently counts candidate-construction Arrays and Hashes while GC is disabled for one invocation.',
        'Avoidable allocations are a feasibility estimate: actual optimized allocations must be measured again after implementation.',
        'Three cases use ordinary prepared execution; physical-plan reuse may legitimately bypass fresh search.',
        'The five-table fixture is separate, bounded untimed coverage; the thirteen-job timing plan is unchanged.'] }
    File.write(options[:report], JSON.generate(report) + "\n")
    puts "Frontier opportunity probe passed #{records.length} jobs / #{records.sum { |record| record[:cases].length }} cases"
  ensure
    sessions&.each(&:close)
    database&.close
  end
end

NWayFrontierOpportunity.main if $PROGRAM_NAME == __FILE__
