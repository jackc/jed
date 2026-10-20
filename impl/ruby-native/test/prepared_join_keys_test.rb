# frozen_string_literal: true

require 'tmpdir'
require 'weakref'
require_relative '../lib/jed_native'

# Structural ownership and retained-metadata accounting are internal cache
# contracts. Shared SQL transcripts remain the behavioral planner oracle.
module NativePreparedJoinKeysTests
  module_function

  APIS = %i[query_prepared execute_prepared query execute].freeze
  LIMITS = JedNative::PreparedJoins

  def assert(condition, message) = (raise(message) unless condition)

  def frame
    entry = LIMITS::Entry.new(tables: [], context: [], roots: {}, columns: {}, scopes: {}, nodes: {},
      scope_columns: {}, key_units: 0, key_bytes: 0, sources: {}, source_units: 0, source_bytes: 0)
    LIMITS::Frame.new(entry: entry, depth: 0, plans: {}, physical_reuse: true)
  end

  def literal(value = 1) = { kind: :literal, type: 'i32', value: value }

  def publication(engine, active, columns, node = literal)
    engine.send(:prepared_join_root, active, :value, node, columns) do
      JedNative::Expression.new(kind: :literal, type: 'i32', value: node[:value] || 1)
    end
  end

  def meters(value)
    units, bytes = 1, value.is_a?(String) ? value.bytesize : 0
    children = case value
               when Array then value
               when Hash then value.flat_map { |key, child| [key, child] }
               else []
               end
    children.each do |child|
      extra_units, extra_bytes = meters(child)
      units += extra_units
      bytes += extra_bytes
    end
    [units, bytes]
  end

  def owned?(value)
    case value
    when Hash then value.frozen? && value.all? { |key, child| owned?(key) && owned?(child) }
    when Array then value.frozen? && value.all? { |child| owned?(child) }
    when String then value.frozen?
    else true
    end
  end

  def accounting(entry)
    # Every root includes its retained staging-array unit. Scope references
    # cost a unit each, while complete descriptor payloads and pool references
    # are charged only once per canonical descriptor.
    units, bytes = entry.roots.length, 0
    entry.roots.each_key do |key|
      count, size = meters(key)
      units += count
      bytes += size
    end
    entry.nodes.each_key do |node|
      count, size = meters(node)
      units += count
      bytes += size
      assert(owned?(node), 'syntax key borrowed mutable metadata')
    end
    entry.scopes.each_key do |scope|
      units += 1 + scope.length
      assert(scope.frozen? && scope.all? { |column| entry.scope_columns[column].equal?(column) },
        'ordered scope does not use canonical owned column descriptors')
    end
    entry.scope_columns.each do |key, column|
      count, size = meters(column)
      units += count + 2
      bytes += size
      assert(key.equal?(column) && owned?(column), 'column pool retained a mutable or duplicate descriptor')
    end
    assert([entry.key_units, entry.key_bytes] == [units, bytes],
      "retained key accounting differs: #{[entry.key_units, entry.key_bytes]} != #{[units, bytes]}")
    assert(units <= LIMITS::KEY_UNITS && bytes <= LIMITS::KEY_BYTES && entry.roots.length <= LIMITS::ROOT_ENTRIES,
      'compact key metadata exceeded its unchanged allowances')
    [units, bytes]
  end

  def state(entry)
    [entry.roots.dup, entry.nodes.dup, entry.scopes.dup, entry.scope_columns.dup,
      entry.key_units, entry.key_bytes]
  end

  def structural_scopes
    JedNative.memory do |database|
      current = database.session
      engine, active = current.engine, frame
      first = { name: +'x', table: +'a', type: 'i32', nullable: false, type_mod: [7, 2],
        source_ordinal: 0, collation: nil, +'extra_key' => { note: +'original' } }
      second = { name: +'x', table: +'b', type: 'i32', nullable: true, source_ordinal: 0 }
      columns = [first, second]
      resolved = publication(engine, active, columns)
      assert(publication(engine, active, Marshal.load(Marshal.dump(columns))).equal?(resolved),
        'equal fresh structural descriptors missed an existing root')
      publication(engine, active, columns.reverse)
      publication(engine, active, [first, first, second])
      assert(active.entry.scopes.length == 3 && active.entry.scope_columns.length == 2 && active.entry.nodes.length == 1,
        'scope order/duplicates or shared descriptor identities were lost')
      scopes = active.entry.scopes.keys
      assert(scopes[0][0].equal?(scopes[1][1]) && scopes[0][0].equal?(scopes[2][0]) &&
        scopes[2][0].equal?(scopes[2][1]), 'equivalent columns were copied across ordered scopes')
      original = scopes[0][0]
      first[:name].replace('changed')
      first[:table].replace('renamed')
      first[:type_mod][0] = 9
      first['extra_key'][:note].replace('changed')
      publication(engine, active, columns)
      assert(original[:name] == 'x' && original[:table] == 'a' && original[:type_mod] == [7, 2] &&
        original['extra_key'][:note] == 'original', 'caller mutation changed a canonical descriptor')
      assert(active.entry.scope_columns.length == 3, 'changed complete descriptor reused the old identity')
      independent = frame
      publication(engine, independent, [original])
      assert(!independent.entry.scope_columns.keys.first.equal?(original),
        'descriptor ownership escaped its statement entry')
      mutable_key = [+'key_part']
      keyed = frame
      publication(engine, keyed, [{ name: 'keyed', type: 'i32', mutable_key => 'payload' }])
      owned_key = keyed.entry.scope_columns.keys.first.keys.find { |key| key.is_a?(Array) }
      mutable_key.first.replace('changed_key')
      assert(owned_key == ['key_part'] && owned?(owned_key), 'compound metadata Hash key was borrowed')
      accounting(keyed.entry)

      base = { name: 'value', table: 't', type: 'i32' }
      variants = [base, base.merge(nullable: nil), base.merge(nullable: false), base.merge(nullable: true),
        base.merge(collation: nil), base.merge(collation: 'C'), base.merge(type_mod: [2, 1]),
        base.merge(native_body_type: 'i16'), base.merge(qualified_only: true), base.merge(source_ordinal: 1),
        base.merge(type: 'i64'), base.merge(table: 'u'), base.merge(name: 'different')]
      roots = variants.map { |column| publication(engine, active, [column]) }
      assert(roots.map(&:object_id).uniq.length == variants.length,
        'a full structural metadata distinction was omitted from scope equality')
      accounting(active.entry)

      engine.instance_variable_set(:@native_join_resolution_frame, active)
      node = { kind: :column, name: 'x', table: 'a' }
      ordinary = [{ name: 'x', table: 'a', type: 'i16' }, { name: 'x', table: 'b', type: 'i64' }]
      left = engine.compile(node, ordinary)
      right = engine.compile(node, ordinary.reverse)
      assert(left.value == 0 && right.value == 1 && left.type == right.type && left.type == 'i16',
        'shared descriptors changed physical column offsets')
      text = engine.compile({ kind: :column, name: 'text' }, [{ name: 'text', type: 'text', collation: 'C' }])
      assert(text.instance_variable_get(:@native_collation)&.name == 'C', 'column collation metadata was lost')
      accounting(active.entry)
    ensure
      engine&.instance_variable_set(:@native_join_resolution_frame, nil)
      current&.close
    end
  end

  def atomic_publication
    JedNative.memory do |database|
      current = database.session
      engine = current.engine
      columns = [{ name: 'first', type: 'i32' }, { name: 'second', type: 'i64', note: 'payload' }]
      measured = frame
      publication(engine, measured, columns)
      required_units, required_bytes = accounting(measured.entry)
      %i[key_units key_bytes].each do |field|
        maximum = field == :key_units ? LIMITS::KEY_UNITS : LIMITS::KEY_BYTES
        required = field == :key_units ? required_units : required_bytes
        [0, 1].each do |excess|
          active = frame
          active.entry.public_send("#{field}=", maximum - required + excess)
          before = state(active.entry)
          publication(engine, active, columns)
          if excess.zero?
            assert(active.entry.roots.length == 1 && active.entry.public_send(field) == maximum,
              'exact retained-key allowance was rejected')
          else
            assert(state(active.entry) == before, 'refused publication retained orphan columns, scopes, nodes or budget')
          end
          assert(active.depth.zero? && active.checks.nil?, 'budget refusal left a compile frame active')
        end
      end

      active = frame
      publication(engine, active, [columns.first])
      before = state(active.entry)
      giant = columns.last.merge(note: 'x' * (LIMITS::KEY_BYTES + 1))
      publication(engine, active, [columns.first, columns.last, giant], literal(2))
      assert(state(active.entry) == before, 'late oversized descriptor left partial interned metadata')
      publication(engine, active, [columns.first, columns.last], literal(2))
      assert(active.entry.scope_columns.length == 2 && active.entry.roots.length == 2,
        'refused staging prevented a later bounded publication')
      accounting(active.entry)

      late = frame
      late.entry.key_bytes = LIMITS::KEY_BYTES - 32_200
      before_late = state(late.entry)
      late_node = { kind: :literal, type: 'text', value: 'x' * 32_000 }
      assert(engine.send(:prepared_join_nodes?, [late_node]), 'late snapshot fixture failed earlier syntax admission')
      publication(engine, late, [columns.first.merge(note: 'x' * 512)], late_node)
      assert(state(late.entry) == before_late, 'late AST overflow published already-staged descriptors')

      before = state(active.entry)
      begin
        engine.send(:prepared_join_root, active, :value, literal(3), [columns.last]) do
          raise JedNative::Error.new('22012', 'ordered resolver fault')
        end
      rescue JedNative::Error => error
        assert(error.sqlstate == '22012', 'compile fault changed')
      end
      assert(state(active.entry) == before && active.depth.zero? && active.checks.nil?,
        'failed compilation published partial key metadata')
      engine.send(:prepared_join_root, active, :value, literal(4), [columns.last]) do
        engine.charge
        JedNative::Expression.new(kind: :literal, type: 'i32', value: 4)
      end
      assert(state(active.entry) == before, 'metered compilation entered the root cache')
      publication(engine, active, [columns.last], { kind: :call, name: 'random', args: [] })
      assert(state(active.entry) == before && !active.physical_reuse,
        'unsupported syntax published a key or preserved physical admission')

      capped = frame
      LIMITS::ROOT_ENTRIES.times { |index| publication(engine, capped, [columns.first], literal(index)) }
      before = state(capped.entry)
      publication(engine, capped, [columns.last], literal(LIMITS::ROOT_ENTRIES))
      assert(state(capped.entry) == before && capped.entry.roots.length == LIMITS::ROOT_ENTRIES,
        'root ceiling retained an orphan descriptor or extra root')
      accounting(capped.entry)
    ensure
      current&.close
    end
  end

  def unusual_metadata
    JedNative.memory do |database|
      current = database.session
      engine = current.engine
      default_calls = 0
      defaulted = Hash.new { |_hash, _key| default_calls += 1; nil }.merge(name: 'x', type: 'i32')
      identity = { name: 'x', type: 'i32' }.compare_by_identity
      subclass = Class.new(Hash).new.merge(name: 'x', type: 'i32')
      label = JedNative::CollationLabel.new(name: 'C')
      cases = [defaulted, identity, subclass, { name: 'x', type: 'i32', note: Object.new },
        { name: 'x', type: 'text', collation_label: label },
        { name: 'x', type: 'i32', nested: Hash.new(false) },
        { name: 'x', type: 'i32', note: Class.new(String).new('value') }]
      cases.each do |column|
        active = frame
        publication(engine, active, [column])
        assert(state(active.entry) == state(frame.entry), 'unsupported mutable metadata was retained')
      end
      assert(default_calls.zero?, 'metadata admission invoked a Hash default callback')
    ensure
      current&.close
    end
  end

  def setup(database)
    database.execute_script(<<~SQL)
      CREATE TABLE jk_items(id i32 PRIMARY KEY, bucket i32, score i32, label text);
      INSERT INTO jk_items VALUES (1,1,10,'Alpha'),(2,0,20,'Beta'),(3,1,30,'Gamma');
    SQL
  end

  def session(database, disabled: false)
    database.session.tap do |current|
      current.engine.instance_variable_set(:@native_join_resolution_disabled, disabled)
      current.engine.instance_variable_set(:@native_join_physical_disabled, true)
    end
  end

  def image(value)
    case value
    when JedNative::CollatedText then [value.to_s, value.collation_label&.to_h]
    when Array then value.map { |item| image(item) }
    else value
    end
  end

  def outcome(current, sql, statement, params, api)
    values, phase, lifetime = [], :admission, current.lifetime_cost
    result = current.public_send(api, api.to_s.end_with?('_prepared') ? statement : sql, *params)
    phase = :pull
    result.each { |row| values << image(row.to_a) }
    [:rows, values, result.columns, result.column_types, result.cost, current.lifetime_cost - lifetime]
  rescue JedNative::Error => error
    [:error, phase, error.sqlstate, error.message, values, result&.cost || current.engine.cost, current.lifetime_cost - lifetime]
  ensure
    result&.close
  end

  def traced
    calls = 0
    trace = TracePoint.new(:call) do |event|
      calls += 1 if event.defined_class == JedNative::Engine && event.method_id == :compile
    end
    answer = trace.enable { yield }
    [answer, calls]
  end

  def compare(sessions, statements, sql, params, api)
    answers = sessions.zip(statements).map { |current, statement| traced { outcome(current, sql, statement, params, api) } }
    assert(answers[0][0] == answers[1][0], "compact keys changed #{api} #{params.inspect}: #{answers.inspect}")
    sessions.each do |current|
      cache = current.engine.instance_variable_get(:@native_join_resolution_plans) || {}
      assert(cache.length <= LIMITS::CACHE_ENTRIES, 'statement entry bound changed')
      cache.each_value { |entry| accounting(entry) }
      assert(!current.engine.instance_variable_get(:@native_join_resolution_frame), 'resolution frame escaped SELECT')
    end
    answers
  end

  def api_parity(database)
    sessions = nil
    sql = 'SELECT a.id,b.score,c.label,d.bucket,e.score FROM jk_items a JOIN jk_items b ON b.id=a.id ' \
      'JOIN jk_items c ON c.id=b.id JOIN jk_items d ON d.id=c.id JOIN jk_items e ON e.id=d.id ' \
      'WHERE a.id<$1 AND b.id<$1 AND c.id<$1 AND d.id<$1 AND e.id<$1 ORDER BY a.id'
    APIS.each do |api|
      sessions = [session(database), session(database, disabled: true)]
      statements = sessions.map { |current| current.prepare(sql) }
      answers = nil
      [[3], [3], [2], [nil], [0], [3]].each do |params|
        answers = compare(sessions, statements, sql, params, api)
        assert(answers.first.first.first == :rows, 'five-input metadata fixture failed')
      end
      assert(answers[0][1] < answers[1][1], 'warm compact-key roots did not reduce ordinary compilation')
      entries = sessions.first.engine.instance_variable_get(:@native_join_resolution_plans).values
      assert(entries.any? { |entry| entry.scope_columns.length < entry.scopes.keys.sum(&:length) },
        'public planner did not share any descriptor across ordered scopes')
      sessions.first.engine.instance_variable_set(:@native_join_resolution_disabled, true)
      disabled = compare(sessions, statements, sql, [3], api)
      assert(disabled[0][1] == disabled[1][1], 'current bypass reused already-published compact keys')
      sessions.each(&:close)
    end
  ensure
    sessions&.each(&:close)
  end

  def contextual_replay
    JedNative.memory do |database|
      current = database.session
      engine, active = current.engine, frame
      engine.instance_variable_set(:@native_join_resolution_frame, active)
      engine.instance_variable_set(:@native_binding_types, %w[i16 i32])
      engine.instance_variable_set(:@params, [1, 1])
      columns = [{ name: 'small', type: 'i16' }, { name: 'large', type: 'i32' }]
      children = columns.each_with_index.map do |column, index|
        { kind: :binary, op: '+', left: { kind: :column, name: column[:name] }, right: { kind: :param, index: index + 1 } }
      end
      node = { kind: :binary, op: '+', left: children[0], right: children[1] }
      engine.compile(node, columns)
      engine.compile(node, columns.reverse)
      [[32_768, 2**31], [1, 2**31], [-32_769, 1], [nil, nil], [2, 3]].each do |params|
        engine.instance_variable_set(:@params, params)
        results = [active, nil].map do |selected|
          engine.instance_variable_set(:@native_join_resolution_frame, selected)
          before = engine.cost
          begin
            expression = engine.compile(node, columns.reverse)
            [:ok, expression.type, engine.cost - before]
          rescue JedNative::Error => error
            [:error, error.sqlstate, error.message, engine.cost - before]
          end
        end
        assert(results.first == results.last, 'shared metadata changed current contextual checks or fault order')
      end
      assert(active.depth.zero? && active.checks.nil?, 'failed replay kept an active frame')
      accounting(active.entry)
    ensure
      engine&.instance_variable_set(:@native_join_resolution_frame, nil)
      current&.close
    end
  end

  def execution_prefixes(database)
    sessions = nil
    sql = 'SELECT a.id,b.score/$2,c.label FROM jk_items a JOIN jk_items b ON b.id=a.id ' \
      'JOIN jk_items c ON c.id=b.id WHERE a.id<$1 ORDER BY a.id'
    APIS.each do |api|
      sessions = [session(database), session(database, disabled: true)]
      statements = sessions.map { |current| current.prepare(sql) }
      2.times { compare(sessions, statements, sql, [3, 1], api) }
      ordinary = compare(sessions, statements, sql, [3, 1], api).first.first
      failure = compare(sessions, statements, sql, [3, 0], api).first.first
      assert(failure[0] == :error && failure[2] == '22012', 'warm root changed division fault timing')
      cost = ordinary[4]
      %i[max_cost lifetime_max_cost].each do |setting|
        [1, cost / 2, cost - 1, cost, cost + 1].select(&:positive?).uniq.each do |limit|
          sessions.each do |current|
            current.public_send("#{setting}=", setting == :lifetime_max_cost ? current.lifetime_cost + limit : limit)
          end
          compare(sessions, statements, sql, [3, 1], api)
          sessions.each { |current| current.max_cost = current.lifetime_max_cost = 0 }
        end
      end
      assert(compare(sessions, statements, sql, [3, 1], api).first.first == ordinary,
        'a failed warm execution changed later query values or costs')
      sessions.each(&:close)
    end
  ensure
    sessions&.each(&:close)
  end

  def detached
    JedNative.memory do |database|
      current = database.session
      name, nested = +'owned_name', [+'owned_extra']
      column = { name: name, type: 'i32', nested: nested }
      active = frame
      publication(current.engine, active, [column])
      refs = [current, current.engine, column, name, nested, nested.first].map { |value| WeakRef.new(value) }
      current.close
      [active.entry, refs]
    end
  end

  def run
    structural_scopes
    atomic_publication
    unusual_metadata
    contextual_replay
    [false, true].each do |disk|
      Dir.mktmpdir('jed-join-keys-') do |directory|
        database = disk ? JedNative.create(File.join(directory, 'fixture.jed'), locking: :exclusive, work_mem: 1024) : JedNative.memory
        setup(database)
        api_parity(database)
        execution_prefixes(database)
      ensure
        database&.close
      end
    end
    entry, refs = Thread.new { detached }.value
    accounting(entry)
    3.times { GC.start }
    assert(refs.none?(&:weakref_alive?), 'canonical descriptors retained caller metadata or an execution owner')
    puts 'ruby-native prepared JOIN keys: ownership, bounds and reuse passed'
  end
end

NativePreparedJoinKeysTests.run
