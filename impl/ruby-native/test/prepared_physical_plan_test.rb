# frozen_string_literal: true

require 'tmpdir'
require 'weakref'
require_relative '../lib/jed_native'

# A shared transcript cannot retain a statement or overlap suspended host
# cursors. Disable only physical reuse in the independent control: both paths
# still exercise the existing resolved-root cache and ordinary row operators.
module NativePreparedPhysicalPlanTests
  module_function

  APIS = %i[query_prepared execute_prepared query execute].freeze
  TWO = 'FROM pp_a a JOIN pp_b b ON b.id=a.k'
  THREE = "#{TWO} JOIN pp_c c ON c.id=a.id"
  FOUR = "#{THREE} JOIN pp_d d ON d.id=c.id"

  def assert(condition, message) = (raise(message) unless condition)

  def setup(database)
    database.execute_script(<<~SQL)
      CREATE TABLE pp_a(id i32 PRIMARY KEY, k i32, narrow i16, flag boolean);
      CREATE TABLE pp_b(id i32 PRIMARY KEY, v i32, bucket i32);
      CREATE TABLE pp_c(id i32 PRIMARY KEY, v i32);
      CREATE TABLE pp_d(id i32 PRIMARY KEY, v i32);
      INSERT INTO pp_a VALUES(1,2,1,TRUE),(2,3,2,NULL),(3,9,3,FALSE);
      INSERT INTO pp_b VALUES(2,20,0),(3,30,1),(4,40,0);
      INSERT INTO pp_c VALUES(1,100),(2,200),(3,300);
      INSERT INTO pp_d VALUES(1,1000),(2,2000),(3,3000);
      CREATE INDEX pp_b_bucket ON pp_b(bucket);
    SQL
  end

  def session(database, disabled: false, **options)
    database.session(**options).tap { |current| current.engine.instance_variable_set(:@native_join_physical_disabled, disabled) }
  end

  def cache(current) = current.engine.instance_variable_get(:@native_join_resolution_plans) || {}
  def physicals(current) = cache(current).values.filter_map(&:physical)

  def outcome(current, statement, sql, params, api, collect: false)
    lifetime, phase, values = current.lifetime_cost, :admission, []
    result = api.to_s.end_with?('_prepared') ? current.public_send(api, statement, *params) : current.public_send(api, sql, *params)
    phase = :pull
    if result.is_a?(JedNative::Rows) && !collect
      result.each { |row| values << row.to_a }
    else
      values = result.rows
    end
    [:rows, values, result.columns, result.column_types, result.cost, current.lifetime_cost - lifetime]
  rescue JedNative::Error => error
    [:error, phase, error.sqlstate, error.message, values, result&.cost || current.engine.cost, current.lifetime_cost - lifetime]
  ensure
    result&.close
  end

  def plan_signature(plan)
    return unless plan.is_a?(Hash)
    access = ->(item) { item&.slice(:kind, :name, :scan_cost, :scan_rows, :output_rows, :logical_rows, :pipeline_cost) }
    if plan.key?(:order)
      plan.slice(:order, :mask, :ordered, :target, :cost, :rows, :logical, :output_rows, :join_cost).merge(
        accesses: plan[:accesses].map(&access),
        steps: plan[:steps].map { |step| step.slice(:algorithm, :kind, :cost, :rows, :logical, :pairs, :prefix_cost, :work).merge(index: access.call(step.dig(:index, :access))) })
    else
      plan.slice(:reverse, :algorithm, :ordered, :target, :cost, :rows, :join_rows, :output_count).merge(
        scans: plan[:scans].map(&access), index: access.call(plan.dig(:index, :access)))
    end
  end

  def traced
    calls, plans = Hash.new(0), []
    trace = TracePoint.new(:call, :return) do |event|
      if event.event == :call
        calls[:two] += 1 if event.defined_class == JedNative::JoinPlanner && event.method_id == :two_base_join_plan
        calls[:multiway] += 1 if event.defined_class == JedNative::NWayJoins && event.method_id == :nway_join_plan
      elsif event.defined_class == JedNative::PreparedJoins && %i[two_base_join_plan nway_join_plan].include?(event.method_id)
        signature = plan_signature(event.return_value)
        plans << signature if signature
      end
    end
    value = trace.enable { yield }
    [value, calls, plans.uniq]
  end

  def compare_current(enabled, disabled, statements, sql, params, api, **options)
    actual, calls, plans = traced { outcome(enabled, statements[0], sql, params, api, **options) }
    expected, ordinary, fresh_plans = traced { outcome(disabled, statements[1], sql, params, api, **options) }
    assert(actual == expected, "physical reuse changed #{api} #{params.inspect} #{sql}: #{actual.inspect} != #{expected.inspect}")
    assert(plans == fresh_plans, "physical winner/estimates changed #{params.inspect}: #{plans.inspect} != #{fresh_plans.inspect}")
    assert(physicals(disabled).empty?, 'physical-only control retained a winner')
    @last_calls, @last_ordinary, @last_plans = calls, ordinary, plans
    actual
  end

  def compare(database, sql, params, api: :query_prepared, warm_params: params, setting: nil, limit: 0, **options)
    enabled, disabled = session(database), session(database, disabled: true)
    statements = [enabled, disabled].map { |current| current.prepare(sql) }
    2.times { compare_current(enabled, disabled, statements, sql, warm_params, api, **options) }
    [enabled, disabled].each do |current|
      current.public_send("#{setting}=", setting == :lifetime_max_cost ? current.lifetime_cost + limit : limit) if setting
    end
    compare_current(enabled, disabled, statements, sql, params, api, **options)
  ensure
    enabled&.close
    disabled&.close
  end

  def blind_reuse_and_null(database)
    enabled = disabled = cursor = nil
    cases = [
      "SELECT a.id,b.v #{TWO} WHERE a.id=$1 ORDER BY a.id",
      "SELECT a.id,b.v,c.v #{THREE} WHERE a.id=$1 ORDER BY a.id",
      "SELECT a.id,b.v,c.v,d.v #{FOUR} WHERE a.id=$1 ORDER BY a.id",
      'SELECT a.id,b.v FROM pp_a a CROSS JOIN pp_b b WHERE a.k=b.id AND a.id=$1 ORDER BY a.id',
      'SELECT a.id,b.v,c.v FROM pp_a a,pp_b b,pp_c c WHERE a.k=b.id AND c.id=a.id AND a.id=$1 ORDER BY a.id'
    ]
    APIS.each do |api|
      cases.each do |sql|
        enabled, disabled = session(database), session(database, disabled: true)
        statements = [enabled, disabled].map { |current| current.prepare(sql) }
        compare_current(enabled, disabled, statements, sql, [nil], api)
        assert(physicals(enabled).empty?, 'NULL-first execution published a physical winner')
        2.times { compare_current(enabled, disabled, statements, sql, [1], api) }
        saved = physicals(enabled).first
        assert(saved && cache(disabled).any?, 'fixture missed physical reuse or disabled control bypassed resolution too')
        [[2], [99], [nil], [1]].each do |params|
          compare_current(enabled, disabled, statements, sql, params, api)
          assert(physicals(enabled).first.equal?(saved), 'binding or NULL fallback replaced the blind template')
          if params.first
            assert(@last_calls.values.sum.zero? && @last_ordinary.values.sum.positive?, 'non-NULL physical hit still ran the base planner')
          end
        end
        enabled.engine.instance_variable_set(:@native_join_physical_disabled, true)
        compare_current(enabled, disabled, statements, sql, [2], api)
        assert(@last_calls == @last_ordinary, 'physical bypass ignored an already-warmed template')
        enabled.engine.instance_variable_set(:@native_join_physical_disabled, false)
        compare_current(enabled, disabled, statements, sql, [1], api)
        cursor = enabled.query_prepared(statements.first, 1)
        assert(cursor.cost.zero?, 'physical plan hit executed during cursor admission')
        cursor.columns.first.replace('caller_name')
        cursor.column_types.first.replace('text')
        cursor.rows
        cursor.close
        compare_current(enabled, disabled, statements, sql, [1], api)
        enabled.close
        disabled.close
      end
    end
  ensure
    cursor&.close
    enabled&.close
    disabled&.close
  end

  def sensitive_bindings(database)
    enabled = disabled = nil
    cases = [
      ["SELECT a.id,b.v,c.v #{THREE} WHERE a.id=$1 AND a.id=$2 ORDER BY a.id", [[1,1],[1,2],[2,2],[nil,2],[1,1]]],
      ["SELECT a.id,b.v,c.v #{THREE} WHERE a.id>=$1 AND a.id<=$2 ORDER BY a.id", [[1,2],[2,1],[2,2],[nil,2],[1,2]]],
      ["SELECT a.id,b.v,c.v #{THREE} WHERE $1 AND a.id=$2 ORDER BY a.id", [[true,1],[false,1],[nil,1],[true,2],[true,1]]],
      ["SELECT a.id,b.v,c.v #{THREE} WHERE $1=$2 ORDER BY a.id", [[1,1],[1,2],[nil,1],[1,1]]]
    ]
    cases.each do |sql, bindings|
      enabled, disabled = session(database), session(database, disabled: true)
      statements = [enabled, disabled].map { |current| current.prepare(sql) }
      2.times { bindings.each { |params| compare_current(enabled, disabled, statements, sql, params, :query_prepared) } }
      # Contradiction pruning runs before physical selection. Its rewritten
      # key may safely admit a value-blind remainder; complete parity above
      # proves that a different contradiction cannot reuse the old remainder.
      enabled.close
      disabled.close
    end
  ensure
    enabled&.close
    disabled&.close
  end

  def analyzed_sensitivity(database)
    writer, enabled, disabled = database.session, nil, nil
    writer.execute_script(<<~SQL)
      CREATE TABLE pp_skew(id i32 PRIMARY KEY,bucket i32);
      CREATE TABLE pp_single(id i32 PRIMARY KEY);
      INSERT INTO pp_single VALUES(1);
      CREATE INDEX pp_skew_bucket ON pp_skew(bucket);
    SQL
    writer.execute("INSERT INTO pp_skew VALUES #{(1..160).map { |id| "(#{id},#{id <= 144 ? 1 : id})" }.join(',')}")
    writer.execute('ANALYZE pp_skew')
    enabled, disabled = session(database), session(database, disabled: true)
    sql = 'SELECT a.id,b.id,c.id FROM pp_skew a CROSS JOIN pp_single b CROSS JOIN pp_single c WHERE a.bucket=$1 AND b.id=1 AND c.id=1 ORDER BY a.id'
    statements = [enabled, disabled].map { |current| current.prepare(sql) }
    winners = []
    [1,160,999,nil,1,160].each do |value|
      compare_current(enabled, disabled, statements, sql, [value], :query_prepared)
      winners.concat(@last_plans.map do |plan|
        [plan[:order], plan[:accesses].map { |access| access.values_at(:kind, :name) }]
      end)
      assert(physicals(enabled).empty?, "MCV/absence value read retained a physical winner for #{value.inspect}")
    end
    assert(winners.uniq.length > 1, 'MCV fixture did not change physical order or scan access across bindings')
  ensure
    enabled&.close
    disabled&.close
    writer&.close
  end

  def warmed_validation_replay(database)
    current = session(database, disabled: true)
    sql = "SELECT a.id,b.v,c.v #{THREE} WHERE a.id=$1"
    statement = current.prepare(sql)
    2.times { current.query_prepared(statement, 1).rows }
    assert(cache(current).values.any? { |entry| entry.roots.any? } && physicals(current).empty?, 'validation fixture did not warm only resolution')
    current.engine.instance_variable_set(:@native_join_physical_disabled, false)
    current.query_prepared(statement, 1).rows
    saved = physicals(current).first
    assert(saved && !saved.checks.empty?, 'physical fill after root hits lost contextual validation logs')
    cold = session(database)
    cold.query_prepared(cold.prepare(sql), 1).rows
    assert(physicals(cold).first&.checks == saved.checks, 'warm-root physical fill changed validation order versus fresh resolution')
    engine = current.engine
    engine.instance_variable_set(:@native_binding_types, ['i32'])
    engine.instance_variable_set(:@params, [2])
    frame = JedNative::PreparedJoins::Frame.new(entry: cache(current).values.first, depth: 0, plans: {})
    observed = []
    engine.define_singleton_method(:cast_value) do |value, type|
      observed << [value, type]
      super(value, type)
    end
    result = engine.send(:prepared_join_physical_plan, frame, saved.key) { raise 'warmed validation missed the physical template' }
    assert(result.equal?(saved.plan) && observed == saved.checks.map { |index,type| [[2][index],type] }, 'physical hit skipped or reordered contextual validation')
    assert(frame.physical_observation.nil? && frame.entry.physical.equal?(saved), 'physical replay changed its observation or template')
  ensure
    current&.close
    cold&.close
  end

  def transaction_and_temp_bypass(database)
    enabled, disabled = session(database), session(database, disabled: true)
    sql = "SELECT a.id,b.v,c.v #{THREE} WHERE a.id=$1"
    statements = [enabled, disabled].map { |current| current.prepare(sql) }
    2.times { compare_current(enabled, disabled, statements, sql, [1], :query_prepared) }
    saved = physicals(enabled).first
    [enabled, disabled].each(&:begin)
    compare_current(enabled, disabled, statements, sql, [2], :query_prepared)
    assert(@last_calls.values.sum.positive?, 'transaction-bound cursor reused a committed physical template')
    cursor = enabled.query_prepared(statements.first, 1)
    assert(cursor.block_bound? && !cursor.instance_variable_get(:@engine).transaction_open?, 'transaction cursor fixture did not exercise a transaction-free cloned engine')
    _, calls, = traced { cursor.rows }
    assert(calls.values.sum.positive?, 'block-bound cloned engine reused a physical template')
    cursor.close
    assert(physicals(enabled).all? { |entry| entry.equal?(saved) }, 'read-only working transaction replaced committed physical metadata')
    [enabled, disabled].each(&:rollback)
    [enabled, disabled].each do |current|
      current.execute('CREATE TEMP TABLE pp_temporary(id i32 PRIMARY KEY)')
      current.execute('INSERT INTO pp_temporary VALUES(1)')
    end
    temp = 'SELECT a.id,b.id FROM pp_temporary a JOIN pp_temporary b ON a.id=b.id WHERE a.id=$1'
    prepared = [enabled, disabled].map { |current| current.prepare(temp) }
    2.times { compare_current(enabled, disabled, prepared, temp, [1], :query_prepared) }
    entries = cache(enabled).values.select { |entry| entry.tables.any? { |_,table| table[:temporary] } }
    assert(entries.any? && entries.all? { |entry| entry.physical.nil? }, 'temporary relation entered persistent physical cache')
  ensure
    cursor&.close
    [enabled, disabled].compact.each do |current|
      current.rollback if current.engine.transaction_open?
      current.close
    end
  end

  def snapshot_bounds
    JedNative.memory do |database|
      current = session(database)
      engine = current.engine
      borrowed = {rows: [[+'table-owned']]}
      entry = JedNative::PreparedJoins::Entry.new(tables: [['borrowed', borrowed]])
      source = {nested: [+'owned'], table: borrowed}
      expression = JedNative::Expression.new(kind: :param, type: 'i32', value: 0)
      source[:expression] = expression
      copy, units, bytes = engine.send(:prepared_join_physical_snapshot, source, entry)
      source[:nested].first.replace('changed')
      source[:nested] << 'later'
      assert(copy.frozen? && copy[:nested].frozen? && copy[:nested].first.frozen? && copy[:nested] == ['owned'], 'physical snapshot retained mutable plan-owned containers')
      assert(copy[:table].equal?(borrowed) && copy[:expression].equal?(expression) && !expression.frozen?, 'physical snapshot copied or froze borrowed table/expression identities')
      assert(units.positive? && bytes.positive?, 'physical snapshot did not account retained metadata')
      maximum = JedNative::PreparedJoins::PHYSICAL_UNITS
      assert(engine.send(:prepared_join_physical_snapshot, Array.new(maximum - 1, 0), entry), 'physical exact unit cap was rejected')
      assert(engine.send(:prepared_join_physical_snapshot, Array.new(maximum, 0), entry).nil?, 'physical unit cap was exceeded')
      maximum = JedNative::PreparedJoins::PHYSICAL_BYTES
      assert(engine.send(:prepared_join_physical_snapshot, 'x' * maximum, entry), 'physical exact byte cap was rejected')
      assert(engine.send(:prepared_join_physical_snapshot, 'x' * (maximum + 1), entry).nil?, 'physical byte cap was exceeded')
      assert(engine.send(:prepared_join_physical_snapshot, ['x' * (maximum / 2), 'y' * (maximum / 2 + 1)], entry).nil?, 'physical string bytes were not cumulative')
      assert(engine.send(:prepared_join_physical_snapshot, JedNative::Relation.new(columns: [], rows: []), entry).nil?, 'physical snapshot retained an execution Relation')
      observation = {sensitive: false, checks: []}
      JedNative::PreparedJoins::PHYSICAL_CHECKS.times { engine.send(:prepared_join_physical_check, observation, [0,'i32']) }
      assert(!observation[:sensitive] && observation[:checks].length == JedNative::PreparedJoins::PHYSICAL_CHECKS, 'exact physical validation cap was rejected')
      engine.send(:prepared_join_physical_check, observation, [0,'i32'])
      assert(observation[:sensitive] && observation[:checks].length == JedNative::PreparedJoins::PHYSICAL_CHECKS, 'physical validation log exceeded its cap')
      frame = JedNative::PreparedJoins::Frame.new(entry: entry, depth: 0, plans: {})
      engine.instance_variable_set(:@native_join_resolution_frame, frame)
      engine.instance_variable_set(:@native_binding_types, ['i32'])
      engine.instance_variable_set(:@params, [1])
      key = [:observation]
      engine.send(:prepared_join_physical_plan, frame, key) do
        {value: engine.send(:plan_constant, expression)}
      end
      assert(entry.physical.nil? && frame.physical_observation.nil?, 'full parameter read entered physical cache or leaked observation state')
      engine.send(:prepared_join_physical_plan, frame, key) do
        {null: engine.send(:plan_constant_null?, expression)}
      end
      saved = entry.physical
      assert(saved && saved.plan == {null: false}, 'NULL-only parameter check prevented blind admission')
      engine.instance_variable_set(:@params, [2])
      assert(engine.send(:prepared_join_physical_plan, frame, key) { raise 'NULL-only template missed a non-NULL value' }.equal?(saved.plan), 'NULL-only template changed after a different binding')
      engine.instance_variable_set(:@params, [nil])
      result = engine.send(:prepared_join_physical_plan, frame, key) { {null: true} }
      assert(result == {null: true} && entry.physical.equal?(saved), 'NULL fallback reused or replaced the blind template')
      engine.instance_variable_set(:@params, [1])
      begin
        engine.send(:prepared_join_physical_plan, frame, [:failing]) { raise JedNative::Error.new('22012','probe failure') }
      rescue JedNative::Error => error
        assert(error.sqlstate == '22012', 'failed planning changed its error')
      end
      assert(frame.physical_observation.nil? && entry.physical.equal?(saved), 'failed selection leaked observation state or replaced the template')
      engine.instance_variable_set(:@native_binding_types, %w[i32 i32])
      engine.instance_variable_set(:@params, [1, 2])
      validations = [[1, 'i16'], [0, 'i32'], [1, 'i16'], [0, 'i64'], [0, 'i32']]
      engine.send(:prepared_join_physical_plan, frame, [:deduplicated]) do
        validations.each { |check| engine.send(:prepared_join_physical_check, frame.physical_observation, check) }
        {validations: true}
      end
      assert(entry.physical.checks == [[1, 'i16'], [0, 'i32'], [0, 'i64']], 'physical validation deduplication lost first-occurrence order or merged distinct casts')
      observed = []
      engine.define_singleton_method(:cast_value) do |value, type|
        observed << [value, type]
        super(value, type)
      end
      engine.send(:prepared_join_physical_plan, frame, [:deduplicated]) { raise 'deduplicated physical template missed' }
      assert(observed == [[2, 'i16'], [1, 'i32'], [1, 'i64']], 'physical replay changed distinct validation order')
      conversions = []
      text = +'7'
      text.define_singleton_method(:to_s) { conversions << :to_s; super() }
      engine.instance_variable_set(:@native_binding_types, %w[text i32])
      engine.instance_variable_set(:@params, [text, 4])
      text_checks = [[1, 'i16'], [0, 'i32'], [0, 'i32'], [1, 'i16'],
        [1, 'text'], [1, 'text'], [0, 'text'], [0, 'text']]
      engine.send(:prepared_join_physical_plan, frame, [:text_validations]) do
        text_checks.each { |check| engine.send(:prepared_join_physical_check, frame.physical_observation, check) }
        {validations: true}
      end
      expected = text_checks.each_with_index.reject { |_, index| index == 3 }.map(&:first)
      assert(entry.physical.checks == expected, 'text source/target validations were deduplicated or reordered')
      observed.clear
      engine.send(:prepared_join_physical_plan, frame, [:text_validations]) { raise 'text validation template missed' }
      assert(observed == expected.map { |index, type| [[text, 4][index], type] }, 'physical replay lost repeated text conversions')
      assert(conversions == [:to_s] * 6, 'physical validation replay changed String singleton conversion observations')
      engine.instance_variable_set(:@native_join_resolution_frame, nil)
      current.close
    end
  end

  def attachment_publication(database)
    writer, enabled, disabled = database.session, session(database), session(database, disabled: true)
    writer.attach('pp_ref')
    writer.execute('CREATE TABLE pp_ref.t(id i32 PRIMARY KEY,v i32)')
    writer.execute('INSERT INTO pp_ref.t VALUES(1,501),(2,502)')
    sql = 'SELECT a.id,r.v,c.v FROM pp_a a JOIN pp_ref.t r ON r.id=a.id JOIN pp_c c ON c.id=a.id WHERE a.id=$1'
    statements = [enabled, disabled].map { |current| current.prepare(sql) }
    2.times { compare_current(enabled, disabled, statements, sql, [1], :query_prepared) }
    saved = physicals(enabled).first
    assert(saved, 'attachment fixture did not exercise physical reuse')
    writer.detach('pp_ref')
    missing = compare_current(enabled, disabled, statements, sql, [1], :query_prepared)
    assert(missing.first == :error && missing[2] == '42P01', 'detached physical table still resolved')
    writer.attach('pp_ref')
    writer.execute('CREATE TABLE pp_ref.t(id i32 PRIMARY KEY,v text)')
    writer.execute("INSERT INTO pp_ref.t VALUES(1,'reattached')")
    result = compare_current(enabled, disabled, statements, sql, [1], :query_prepared)
    assert(result[1] == [[1,'reattached',100]] && result[3] == %w[i32 text i32], 'reattached alias reused old physical values or metadata')
    assert(physicals(enabled).none? { |entry| entry.equal?(saved) }, 'reattached alias retained old physical identity')
    writer.detach('pp_ref')
  ensure
    writer&.close
    enabled&.close
    disabled&.close
  end

  def cache_bounds
    JedNative.memory do |database|
      setup(database)
      current = session(database)
      (JedNative::PreparedJoins::CACHE_ENTRIES + 4).times do |ordinal|
        sql = "SELECT a.id+#{ordinal},b.v,c.v #{THREE} WHERE a.id=1"
        statement = current.prepare(sql)
        2.times { current.query_prepared(statement).rows }
      end
      entries = cache(current)
      assert(entries.length == JedNative::PreparedJoins::CACHE_ENTRIES && physicals(current).length == entries.length, 'physical statement churn did not reach the bounded cache')
      entries.each_value do |entry|
        physical = entry.physical
        assert(physical.frozen? && physical.key.frozen? && physical.plan.frozen? && physical.checks.frozen?, 'physical record or root metadata remained mutable')
        assert(physical.units <= JedNative::PreparedJoins::PHYSICAL_UNITS && physical.bytes <= JedNative::PreparedJoins::PHYSICAL_BYTES &&
          physical.checks.length <= JedNative::PreparedJoins::PHYSICAL_CHECKS, 'physical entry exceeded a retention bound')
      end
      current.close
    end
  end

  def budgets_and_errors(database)
    sql = "SELECT a.id,b.v/$1,c.v #{THREE} WHERE a.id=$2 ORDER BY a.id"
    %i[query_prepared execute_prepared].each do |api|
      [[2,1],[0,1],[0,99],[nil,1]].each do |params|
        result = compare(database, sql, params, api: api, warm_params: [2,1])
        total = result[-2]
        %i[max_cost lifetime_max_cost].each do |setting|
          (1..total + 1).each do |ceiling|
            compare(database, sql, params, api: api, warm_params: [2,1], setting: setting, limit: ceiling)
          end
        end
      end
      [1,63,64,127,128,255,256,511,512,1024,65_536].each do |ceiling|
        [false,true].each do |collect|
          compare(database, sql, [2,1], api: api, warm_params: [2,1], setting: :max_query_memory_bytes, limit: ceiling, collect: collect)
        end
      end
    end
    APIS.each do |api|
      narrow = "SELECT a.narrow+$1,b.v,c.v #{THREE} WHERE a.id=$2 ORDER BY a.id"
      [[32767,1],[32767,99],[nil,1],[1,1]].each do |params|
        compare(database, narrow, params, api: api, warm_params: [1,1])
      end
    end
  end

  def suspended_and_transactional(database)
    enabled, disabled, writer = session(database), session(database, disabled: true), database.session
    sql = "SELECT a.id+$1::i32,b.v,c.v #{THREE} WHERE a.id=$2 ORDER BY a.id"
    statements = [enabled, disabled].map { |current| current.prepare(sql) }
    2.times { compare_current(enabled, disabled, statements, sql, [0,1], :query_prepared) }
    saved = physicals(enabled).first
    assert(saved, 'suspended snapshot fixture did not retain a physical plan')
    first = [enabled, disabled].each_with_index.map { |current,index| current.query_prepared(statements[index], 1, 1) }
    second = [enabled, disabled].each_with_index.map { |current,index| current.query_prepared(statements[index], 10, 2) }
    assert((first + second).all? { |cursor| cursor.cost.zero? }, 'suspended cursor planned or read rows during admission')
    writer.execute('UPDATE pp_c SET v=299 WHERE id=2')
    fresh = compare_current(enabled, disabled, statements, sql, [100,2], :query_prepared)
    assert(fresh[1] == [[102,30,299]], 'new cursor retained old plan data after publication')
    assert(!physicals(enabled).first.equal?(saved), 'same-row-count publication retained physical snapshot identity')
    assert(second.map(&:rows).uniq == [[[12,30,200]]] && first.map(&:rows).uniq == [[[2,20,100]]],
      'physical templates crossed distinct suspended bindings or pinned snapshots')
    first.each(&:close)
    second.each(&:close)
    committed = physicals(enabled).first
    enabled.begin
    enabled.execute('UPDATE pp_c SET v=301 WHERE id=1')
    assert(outcome(enabled, statements.first, sql, [0,1], :query_prepared)[1] == [[1,20,301]], 'pending physical execution missed own write')
    enabled.execute('SAVEPOINT physical_before_change')
    enabled.execute('UPDATE pp_c SET v=401 WHERE id=1')
    assert(outcome(enabled, statements.first, sql, [0,1], :query_prepared)[1] == [[1,20,401]], 'physical execution reused old pending rows')
    enabled.execute('ROLLBACK TO physical_before_change')
    assert(outcome(enabled, statements.first, sql, [0,1], :query_prepared)[1] == [[1,20,301]], 'physical execution ignored savepoint rollback')
    assert(physicals(enabled).all? { |entry| entry.equal?(committed) }, 'working transaction published a new physical template')
    enabled.rollback
    compare_current(enabled, disabled, statements, sql, [0,1], :query_prepared)
    cursors = [enabled, disabled].each_with_index.map { |current,index| current.query_prepared(statements[index], 0, 1) }
    assert(cursors.map(&:next) == [[1,20,100],[1,20,100]], 'warm physical cursor changed first row')
    costs = cursors.map(&:cost)
    cursors.each(&:close)
    assert(cursors.map(&:cost) == costs && database.instance_variable_get(:@pins).empty?, 'closing physical cursor did more work or retained pins')
  ensure
    (first.to_a + second.to_a + cursors.to_a).each(&:close)
    enabled&.rollback if enabled&.engine&.transaction_open?
    enabled&.close
    disabled&.close
    writer&.close
  end

  def publication(database)
    enabled, disabled, writer = session(database), session(database, disabled: true), database.session
    sql = "SELECT a.id,b.v,c.v #{THREE} WHERE a.id=$1 ORDER BY a.id"
    statements = [enabled, disabled].map { |current| current.prepare(sql) }
    2.times { compare_current(enabled, disabled, statements, sql, [1], :query_prepared) }
    [
      'CREATE TABLE pp_unrelated(id i32)', 'INSERT INTO pp_unrelated VALUES(1)',
      'UPDATE pp_b SET v=21 WHERE id=2', 'ANALYZE pp_b',
      'CREATE INDEX pp_c_v ON pp_c(v)', 'DROP INDEX pp_c_v'
    ].each do |action|
      writer.execute(action)
      [[1],[2],[99],[nil],[1]].each { |params| compare_current(enabled, disabled, statements, sql, params, :query_prepared) }
    end
    [enabled, disabled].each { |current| current.revoke(:select, 'pp_c') }
    denied = compare_current(enabled, disabled, statements, sql, [99], :query_prepared)
    assert(denied.first == :error && denied[2] == '42501', 'warm empty physical join skipped current permissions')
    [enabled, disabled].each(&:reset_privileges)
    [enabled, disabled].each { |current| current.timezone = '+02:00' }
    compare_current(enabled, disabled, statements, sql, [1], :query_prepared)
    writer.execute('DROP TABLE pp_c')
    writer.execute('CREATE TABLE pp_c(id i32 PRIMARY KEY,v text)')
    writer.execute("INSERT INTO pp_c VALUES(1,'replacement')")
    result = compare_current(enabled, disabled, statements, sql, [1], :query_prepared)
    assert(result[1] == [[1,21,'replacement']] && result[3] == %w[i32 i32 text], 'physical template retained replaced schema types or rows')
  ensure
    enabled&.close
    disabled&.close
    writer&.close
  end

  def detached_cache
    JedNative.memory do |database|
      setup(database)
      current = session(database)
      database.execute('ALTER TABLE pp_a ADD COLUMN label text')
      database.execute("UPDATE pp_a SET label='retained' WHERE id=1")
      sql = "SELECT a.id,$1,a.label=$1,b.v,c.v #{THREE} WHERE a.id=$2"
      statement = current.prepare(sql)
      input = +'retained'
      2.times { current.query_prepared(statement, input, 1).rows }
      cursor = current.query_prepared(statement, input, 1)
      execution = cursor.instance_variable_get(:@engine)
      cursor.rows
      retained = cache(current)
      references = [current, current.engine, cursor, execution, input].map { |value| WeakRef.new(value) }
      current.close
      [retained, references]
    end
  end

  def retention
    retained, references = Thread.new { detached_cache }.value
    assert(retained.values.any?(&:physical), 'physical lifetime probe did not retain a template')
    3.times { GC.start }
    assert(references.none?(&:weakref_alive?), 'physical template retained a session, cursor, execution engine or host binding')
  end

  def run
    %i[memory file].each do |mode|
      runner = lambda do |database|
        setup(database)
        blind_reuse_and_null(database)
        sensitive_bindings(database)
        analyzed_sensitivity(database)
        warmed_validation_replay(database)
        transaction_and_temp_bypass(database)
        budgets_and_errors(database)
        suspended_and_transactional(database)
        attachment_publication(database)
        publication(database)
      end
      if mode == :memory
        JedNative.memory(&runner)
      else
        Dir.mktmpdir('jed-ruby-prepared-physical') do |directory|
          JedNative.create(File.join(directory, 'db'), page_size: 512, locking: :exclusive, &runner)
        end
      end
    end
    snapshot_bounds
    cache_bounds
    retention
    puts 'ruby-native prepared physical joins: value-blind reuse, binding-sensitive fallback, exact costs/errors, publication, transactions and cursor ownership passed'
  end
end

NativePreparedPhysicalPlanTests.run
