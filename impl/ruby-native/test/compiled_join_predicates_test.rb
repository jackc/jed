# frozen_string_literal: true

require 'tmpdir'
require 'weakref'
require_relative '../lib/jed_native'

# Shared join transcripts own SQL behavior. These probes observe the new
# evaluation seam, including independent ON trees, compilation admission,
# mutable resolved inputs, exact guard prefixes and captured execution state.
module NativeCompiledJoinPredicateTests
  APIS = %i[query_prepared execute_prepared query execute].freeze
  COLUMNS = [
    { table: 'l', name: 'id', type: 'i32' },
    { table: 'l', name: 'flag', type: 'boolean' },
    { table: 'r', name: 'id', type: 'i32' },
    { table: 'r', name: 'flag', type: 'boolean' }
  ].freeze
  module_function

  def assert(value, message) = (raise message unless value)

  def engine(disabled: false, params: [], types: [])
    JedNative::Engine.new.tap do |current|
      current.instance_variable_set(:@native_join_compilation_disabled, disabled)
      current.instance_variable_set(:@params, params)
      current.instance_variable_set(:@native_binding_types, types)
    end
  end

  def expression(sql, columns: COLUMNS, types: [], filter: true)
    current = engine(types: types, params: Array.new(types.length))
    ast = JedNative::Statement.new(filter ? "SELECT 1 WHERE #{sql}" : "SELECT #{sql}").ast
    resolved = filter ? current.compile_filter(ast[:where], columns) : current.compile(ast[:columns].first[:expr], columns)
    [resolved, current.instance_variable_get(:@types)]
  end

  def metadata(value)
    case value
    when Array then value.map { |item| metadata(item) }
    when JedNative::CollatedText then [value.class.name, value.to_s, value.collation_label.to_h]
    when String then [value.class.name, value.to_s]
    else value
    end
  end

  class ProbeRow
    def initialize(values, events, failure)
      @values, @events, @failure = values, events, failure
    end
    def [](ordinal)
      @events << [:read, ordinal]
      raise JedNative::Error.new('XX001', 'join deferred column failed') if ordinal == @failure
      @values[ordinal]
    end
  end

  module Charges
    attr_accessor :join_events
    def charge(amount = 1)
      join_events << [:charge, amount, cost]
      super
    end
  end

  def scalar_outcome(node, kernel, values, params, types, failure, statement, lifetime, prior)
    current = engine(params: params, types: types)
    current.singleton_class.prepend(Charges)
    events = current.join_events = []
    current.max_cost, current.lifetime_max_cost = statement, lifetime
    current.instance_variable_set(:@lifetime_cost, prior)
    row = ProbeRow.new(values, events, failure)
    begin
      value = kernel ? kernel.call(current, row, params) : current.evaluate(node, row)
      result = [:value, metadata(value)]
    rescue JedNative::Error => error
      result = [:error, error.sqlstate, error.message]
    end
    [*result, current.cost, current.lifetime_cost, events]
  end

  def scalar_compare(sql, values: [], params: [], types: [], columns: COLUMNS, filter: true, failure: nil, expected: nil)
    node, registry = expression(sql, columns: columns, types: types, filter: filter)
    program = JedNative::CompiledExpression.prepare(node, types: registry)
    assert(program, "supported join expression rejected: #{sql}")
    kernel = program.materialize
    arguments = [values, params, types, failure]
    original = scalar_outcome(node, nil, *arguments, 0, 0, 0)
    actual = scalar_outcome(node, kernel, *arguments, 0, 0, 0)
    assert(actual == original, "join expression changed #{sql}: #{actual.inspect} != #{original.inspect}")
    assert(actual[0, expected.length] == expected, "unexpected join reference: #{actual.inspect}") if expected
    (1..original[-3] + 1).each do |limit|
      [[limit, 0, 0], [0, limit + 7, 7], [limit, limit + 7, 7]].each do |guards|
        original_prefix = scalar_outcome(node, nil, *arguments, *guards)
        compiled_prefix = scalar_outcome(node, kernel, *arguments, *guards)
        assert(compiled_prefix == original_prefix, "join expression guard/read order changed #{sql}: #{guards.inspect}")
      end
    end
    kernel
  end

  def scalar_semantics
    [true, false, nil].repeated_permutation(2) do |left, right|
      scalar_compare('l.flag AND r.flag', values: [1, left, 1, right])
      scalar_compare('l.flag OR r.flag', values: [1, left, 1, right])
    end
    scalar_compare('l.flag AND 10 / r.id > 0', values: [1, nil, 0, true], expected: [:value, nil])
    scalar_compare('l.flag AND 10 / r.id > 0', values: [1, nil, 0, true], filter: false, expected: [:error, '22012'])
    scalar_compare('(l.flag AND 10 / r.id > 0) OR FALSE', values: [1, nil, 0, true], expected: [:error, '22012'])
    scalar_compare('l.id + r.id > 0', values: [nil, true, 1, false], failure: 2, expected: [:error, 'XX001'])
    scalar_compare('l.flag AND r.flag', values: [1, false, 1, true], failure: 3, expected: [:value, false])
    scalar_compare('CASE WHEN l.flag THEN l.id = r.id ELSE 10 / r.id > 0 END', values: [2, true, 2, nil], expected: [:value, true])
    scalar_compare('l.id + r.id > $1', values: [2_147_483_647, true, 1, false], params: [0], types: ['i32'], expected: [:error, '22003'])
    text_columns = [{ table: 'l', name: 'label', type: 'text' }, { table: 'r', name: 'label', type: 'text' }]
    scalar_compare('l.label < r.label', columns: text_columns, values: ["a\0é", 'ä'])
    scalar_compare("l.label LIKE 'r_w%'", columns: text_columns, values: ['row5', nil], expected: [:value, true])
    # Dynamic labels must still be observed when no static label overrides them.
    label = JedNative::CollationLabel.new(conflict: true).freeze
    scalar_compare('$1 < $2', columns: [], types: %w[text text],
      params: [JedNative::CollatedText.new('a', label), JedNative::CollatedText.new('b', label)], expected: [:error, '42P22'])
  end

  def cache_admission
    current = engine
    node, = expression('l.id + 1 = r.id')
    assert(current.native_compiled_join_predicate(node).nil?, 'join kernel skipped second-use admission')
    first = current.native_compiled_join_predicate(node)
    assert(first && current.native_compiled_join_predicate(node).equal?(first), 'join cache did not reuse exact source and pool')
    equivalent, = expression('l.id + 1 = r.id')
    assert(current.native_compiled_join_predicate(equivalent).equal?(first), 'join cache used expression identity')
    node.args[0].args[1].value = 2
    assert(current.native_compiled_join_predicate(node).nil?, 'changed literal reused an old pool')
    second = current.native_compiled_join_predicate(node)
    assert(second && !second.equal?(first) && first.call(current, [1, nil, 2, nil], []),
      'mutated literal changed retained kernel data')
    assert(second.call(current, [1, nil, 3, nil], []), 'changed resolved expression did not compile')

    current.instance_variable_set(:@native_compilation_immediate, true)
    conjunction, = expression('l.flag AND 10 / r.id > 0')
    filtered = current.native_compiled_join_predicate(conjunction)
    conjunction.remove_instance_variable(:@native_filter_conjunction)
    ordinary = current.native_compiled_join_predicate(conjunction)
    assert(filtered && ordinary && !filtered.equal?(ordinary), 'filter marking was omitted from join cache identity')
    assert(filtered.call(current, [1, nil, 0, nil], []).nil?, 'filter kernel lost its NULL short circuit')
    begin
      ordinary.call(current, [1, nil, 0, nil], [])
      raise 'value AND kernel suppressed its right error'
    rescue JedNative::Error => error
      assert(error.sqlstate == '22012', 'value AND changed error')
    end
    %i[@native_join_compilation_disabled @native_compilation_disabled].each do |flag|
      current.instance_variable_set(flag, true)
      assert(current.native_compiled_join_predicate(node).nil?, "#{flag} did not override a warm join kernel")
      current.instance_variable_set(flag, false)
      assert(current.native_compiled_join_predicate(node).equal?(second), 'disable/re-enable lost valid cache entry')
    end
    text, = expression("l.label = 'same'", columns: [{ table: 'l', name: 'label', type: 'text' }])
    assert(current.native_compiled_join_predicate(text), 'text predicate did not compile')
    current.instance_variable_set(:@native_text_compilation_disabled, true)
    assert(current.native_compiled_join_predicate(text).nil? && current.native_compiled_join_predicate(node),
      'text control failed to isolate text compilation')
    current.instance_variable_set(:@native_text_compilation_disabled, false)

    # Unsupported descendants reject the complete root; no resolved expression
    # is retained as an interpreter fallback in a generated join method.
    unsupported, = expression('upper(l.label) = r.label',
      columns: [{ table: 'l', name: 'label', type: 'text' }, { table: 'r', name: 'label', type: 'text' }])
    assert(current.native_compiled_join_predicate(unsupported).nil?, 'unsupported join subtree partially compiled')
    unsupported.args[0] = unsupported.args[0].args.first
    assert(current.native_compiled_join_predicate(unsupported), 'unsupported result stuck after expression mutation')
    unsupported.args[0].instance_variable_set(:@native_collation, JedNative::CollationLabel.new(name: 'unicode'))
    assert(current.native_compiled_join_predicate(unsupported).nil?, 'unsafe static collation reused a C kernel')

    80.times do |index|
      item, = expression("l.id + #{index + 10} = r.id")
      assert(current.native_compiled_join_predicate(item), 'join cache churn failed admission')
    end
    cache = current.instance_variable_get(:@native_compiled_join_predicates)
    assert(cache.length == JedNative::CompiledPlans::MAX_JOIN_PREDICATE_CACHE_ENTRIES &&
      !cache.values.include?(first), 'join cache exceeded its bound or failed eviction')
    cache.each do |key, kernel|
      assert(key == [kernel.source, kernel.pool] && key.frozen? && kernel.pool.frozen?, 'join cache key is not exact immutable source/pool')
      assert(kernel.source_bytes <= JedNative::CompiledExpression::MAX_SOURCE_BYTES &&
        kernel.node_count <= JedNative::CompiledExpression::MAX_NODES, 'join kernel escaped compiler bounds')
      assert(kernel.pool.none? { |value| value.is_a?(JedNative::Expression) || value.is_a?(Hash) || value.is_a?(Array) },
        'join kernel captured a mutable resolution tree')
    end
    deep = JedNative::Expression.new(kind: :literal, type: 'boolean', value: true)
    40.times { deep = JedNative::Expression.new(kind: :unary, type: 'boolean', value: 'not', args: [deep]) }
    assert(current.native_compiled_join_predicate(deep).nil?, 'join compiler ignored depth limit')
    large, = expression("l.label = 'x'", columns: [{ table: 'l', name: 'label', type: 'text' }])
    large.args[1].value = 'x' * (JedNative::CompiledExpression::MAX_POOL_BYTES + 1)
    assert(current.native_compiled_join_predicate(large).nil?, 'join compiler ignored literal byte limit')
  end

  def acquisition(current, node, input_size)
    preparations = 0
    trace = TracePoint.new(:call) do |event|
      preparations += 1 if event.self.equal?(JedNative::CompiledExpression) && event.method_id == :prepare
    end
    kernel = trace.enable { current.native_compiled_join_predicate(node, input_size: input_size) }
    [kernel, preparations]
  ensure
    trace&.disable
  end

  def simple_admission
    threshold = JedNative::CompiledPlans::MIN_SIMPLE_JOIN_PREDICATE_INPUTS
    assert(threshold == 128 && JedNative::CompiledPlans::MIN_JOIN_PREDICATE_CANDIDATES == 16,
      'join input admission boundaries changed')
    %w[i16 i32 i64].each do |type|
      columns = COLUMNS.map { |column| column[:type] == 'i32' ? column.merge(type: type) : column }
      %w[= <> != < <= > >=].each do |operator|
        current = engine
        node, = expression("l.id #{operator} r.id", columns: columns)
        [0, 15, 16, threshold - 1].each do |size|
          2.times do
            assert(acquisition(current, node, size) == [nil, 0], 'cheap integer predicate reached the emitter')
          end
        end
        assert(!current.instance_variable_defined?(:@native_compiled_join_predicates),
          'rejected cheap predicate occupied a cache entry')
        assert(acquisition(current, node, threshold) == [nil, 1], 'large predicate lost second-use admission')
        kernel, preparations = acquisition(current, node, threshold)
        assert(kernel && preparations == 1, 'large integer predicate did not compile')
        assert(acquisition(current, node, threshold - 1) == [nil, 0],
          'a warmed kernel bypassed the cheap-input guard')
        assert(acquisition(current, node, nil) == [kernel, 1], 'unknown input lost existing admission')
        current.instance_variable_set(:@native_compilation_immediate, true)
        assert(acquisition(current, node, 0) == [kernel, 1], 'immediate compilation did not bypass the input guard')
        %i[@native_compilation_disabled @native_join_compilation_disabled].each do |flag|
          current.instance_variable_set(flag, true)
          assert(acquisition(current, node, threshold) == [nil, 0], "#{flag} lost precedence over immediate compilation")
          current.instance_variable_set(flag, false)
        end
      end
    end

    # Literal and parameter leaves are cheap too. More substantial integer
    # expressions, boolean control flow and C text comparisons remain eligible.
    [
      ['l.id=1', COLUMNS, [], false],
      ['l.id=$1', COLUMNS, ['i32'], false],
      ['$1=$2', [], %w[i32 i32], false],
      ['l.id+1=r.id', COLUMNS, [], true],
      ['l.id::i64=r.id', COLUMNS, [], true],
      ['-l.id=r.id', COLUMNS, [], true],
      ['l.flag AND r.flag', COLUMNS, [], true],
      ['l.id=r.id AND abs(l.id-r.id)<50', COLUMNS, [], true],
      ['l.label=r.label', [{ table: 'l', name: 'label', type: 'text' },
                          { table: 'r', name: 'label', type: 'text' }], [], true]
    ].each do |sql, columns, types, admitted|
      current = engine(types: types)
      node, = expression(sql, columns: columns, types: types)
      cold, first_count = acquisition(current, node, 16)
      warm, second_count = acquisition(current, node, 16)
      assert(cold.nil? && !!warm == admitted && [first_count, second_count] == (admitted ? [1, 1] : [0, 0]),
        "cheap predicate classification changed #{sql}")
    end

    current = engine
    node, = expression('l.id=r.id')
    assert(acquisition(current, node, 16) == [nil, 0], 'initial simple expression was not skipped')
    arithmetic, = expression('l.id+1=r.id')
    original = node.args.first
    node.args[0] = arithmetic.args.first
    assert(acquisition(current, node, 16) == [nil, 1], 'cheap rejection stuck after shape mutation')
    kernel, = acquisition(current, node, 16)
    assert(kernel && kernel.call(current, [1, nil, 2, nil], []), 'mutated arithmetic predicate did not execute')
    node.args[0] = original
    assert(acquisition(current, node, 16) == [nil, 0], 'shape mutation reused the arithmetic kernel')
    text, = expression('l.label=r.label',
      columns: [{ table: 'l', name: 'label', type: 'text' }, { table: 'r', name: 'label', type: 'text' }])
    node.args = text.args
    node.extra = text.extra
    assert(acquisition(current, node, 16) == [nil, 1], 'integer-to-text mutation kept the cheap classification')
    text_kernel, = acquisition(current, node, 16)
    assert(text_kernel && text_kernel.call(current, ['é', 'é'], []), 'mutated text predicate did not execute')
    current.instance_variable_set(:@native_text_compilation_disabled, true)
    assert(acquisition(current, node, 16).first.nil?, 'text disable ignored a mutated warm predicate')

    # The classifier is conservative about custom containers and the current
    # registry. Such shapes still reach the ordinary emitter and its guards.
    node, = expression('l.id=r.id')
    expression_class = Class.new(JedNative::Expression)
    array_class = Class.new(Array)
    [expression_class.new(**node.to_h),
     node.dup.tap { |copy| copy.args = [expression_class.new(**node.args.first.to_h), node.args.last] },
     node.dup.tap { |copy| copy.args = array_class.new(node.args) }].each do |variant|
      owner = engine
      assert(acquisition(owner, variant, 16) == [nil, 1], 'custom expression/container was classified as an exact simple tree')
      assert(acquisition(owner, variant, 16).first, 'custom expression/container lost existing emitter admission')
    end
    %w[boolean i32].each do |type|
      owner = engine
      acquisition(owner, node, threshold)
      old, = acquisition(owner, node, threshold)
      registry = owner.instance_variable_get(:@types)
      registry[type] = { kind: :enum }
      assert(acquisition(owner, node, 16) == [nil, 1] && acquisition(owner, node, threshold) == [nil, 1],
        'registry mutation bypassed the emitter type guard or reused a cached predicate')
      registry.delete(type)
      assert(acquisition(owner, node, 16) == [nil, 0] && acquisition(owner, node, threshold) == [old, 1],
        'restoring the registry lost current-shape classification or a valid kernel')
    end

    # Admission does not evaluate parameters or touch rows. Compare every
    # small cost/lifetime prefix on both sides of the new boundary, including
    # a contextual cast failure and a deferred right-column failure.
    ordinary, = expression('l.id=r.id')
    contextual, = expression('l.id=$1', types: ['i32'])
    contextual.args.last.type = 'i16'
    contextual.args.last.extra = { context_type: 'i16' }
    [[ordinary, [1, nil, 1, nil], [], [], nil],
     [ordinary, [nil, nil, 1, nil], [], [], 2],
     [contextual, [1, nil, nil, nil], [40_000], ['i64'], nil]].each do |predicate, row, params, types, failure|
      [16, threshold - 1, threshold, nil].each do |size|
        owner = engine(params: params, types: types)
        acquisition(owner, predicate, size)
        compiled, = acquisition(owner, predicate, size)
        assert(owner.cost.zero? && owner.lifetime_cost.zero?, 'admission evaluated or charged a predicate')
        expected = scalar_outcome(predicate, nil, row, params, types, failure, 0, 0, 0)
        assert(expected[0, 2] == [:error, '22003'], 'contextual parameter fixture lost its narrowing error') if predicate.equal?(contextual)
        [[0, 0, 0], [1, 0, 0], [0, 8, 7], [1, 8, 7]].each do |guards|
          original = scalar_outcome(predicate, nil, row, params, types, failure, *guards)
          actual = scalar_outcome(predicate, compiled, row, params, types, failure, *guards)
          assert(actual == original, "cheap predicate changed value/error/cost prefix #{[size, guards, expected].inspect}")
        end
      end
    end
  end

  def traced
    kernels, counts, scratch = {}, Hash.new(0), []
    trace = TracePoint.new(:call, :return) do |event|
      if event.event == :return && event.method_id == :native_compiled_join_predicate
        kernel = event.return_value
        kernels[kernel.object_id] = kernel if kernel
      elsif event.event == :call
        counts[:kernel] += 1 if event.method_id == :call && kernels[event.self.object_id].equal?(event.self)
        counts[event.method_id] += 1 if %i[join_index_relation spill_join spill_index_join].include?(event.method_id)
        if event.defined_class == JedNative::Engine && event.method_id == :join
          source = event.binding.local_variable_get(:source)
          counts[:on_trees] += 1 if source[:native_on_trees]
          counts[:window] += 1 if source[:native_join_window]
        elsif event.defined_class == JedNative::ScratchFile && event.method_id == :append
          counts[:scratch] += 1
          scratch << event.self unless scratch.include?(event.self)
        end
      elsif event.method_id == :hash_join_table && event.return_value
        counts[:hash] += 1
      elsif event.defined_class == JedNative::Engine && event.method_id == :join
        programs = event.binding.local_variable_get(:on_programs)
        counts[:on_program_vectors] += 1 if programs
        counts[:on_program_kernels] += programs.count { |_, kernel| kernel } if programs
      end
    end
    value = trace.enable { yield }
    [value, counts, kernels.values, scratch]
  ensure
    trace&.disable
  end

  def tree(sql) = JedNative::Statement.new("SELECT 1 WHERE #{sql}").ast[:where]

  def join_tree_outcome(disabled, trees, window, limit, kind: :left, algorithm: :nested, on: '10 / r.id > 0',
                        immediate: true, expanded: false, lifetime: 0)
    current = engine(disabled: disabled)
    current.instance_variable_set(:@native_compilation_immediate, immediate)
    current.max_cost = limit
    current.lifetime_max_cost = lifetime
    current.instance_variable_set(:@lifetime_cost, 7) if lifetime.positive?
    right = JedNative::Relation.new(columns: COLUMNS.drop(2), rows: [[1, true], [2, false], [0, nil]])
    current.define_singleton_method(:source_relation) { |_| right }
    left = JedNative::Relation.new(columns: COLUMNS.take(2), rows: [[1, true], [2, nil], [3, false]])
    left.rows *= 2 if expanded
    source = { kind: :table, name: 'r', native_join_algorithm: algorithm }
    source[:native_on_trees] = trees unless trees.nil?
    source[:native_join_window] = { target: 1, filter: current.compile_filter(tree(window), COLUMNS) } if window
    unless immediate
      trees&.each do |node|
        current.native_compiled_join_predicate(current.compile_filter(node, COLUMNS),
          input_size: left.rows.length * right.rows.length)
      end
    end
    begin
      result = current.join(left, source, kind, tree(on), nil)
      value = [:rows, result.rows, result.columns]
    rescue JedNative::Error => error
      value = [:error, error.sqlstate, error.message]
    end
    [*value, current.cost, current.lifetime_cost]
  end

  def separate_on_trees
    [
      [[tree('l.flag'), tree('l.id = r.id')], nil],
      [[tree('l.flag'), tree('l.id = r.id')], 'r.id + l.id > 2'],
      [[], nil],
      [[tree('l.id = r.id'), tree('10 / r.id > 0')], nil]
    ].each do |trees, window|
      plain = join_tree_outcome(true, trees, window, 0)
      actual, calls = traced { join_tree_outcome(false, trees, window, 0) }
      assert(actual == plain, 'separate ON trees changed results, cost or window ordering')
      assert(calls[:on_trees].positive?, 'internal ON-tree fixture missed Engine#join')
      assert(calls[:kernel].positive?, 'ON-tree fixture did not execute a generated predicate') unless trees.empty?
      (1..plain[-2] + 1).each do |limit|
        assert(join_tree_outcome(false, trees, window, limit) == join_tree_outcome(true, trees, window, limit),
          "separate ON trees changed cost prefix #{limit}")
      end
    end
    plain = join_tree_outcome(true, nil, nil, 0, algorithm: :hash, on: 'l.id=r.id AND l.id<>2')
    actual, calls = traced { join_tree_outcome(false, nil, nil, 0, algorithm: :hash, on: 'l.id=r.id AND l.id<>2') }
    assert(actual == plain && calls[:hash].positive? && calls[:kernel].positive?,
      "forced hash candidates did not recheck the same ON predicate: #{[plain, actual, calls].inspect}")

    # Known inputs18 decline the direct comparison while retaining arithmetic.
    # The lazy ON program vector must preserve every original all? boundary,
    # including when the first compiled entry follows a declined entry.
    [[tree('l.id=r.id'), tree('10/r.id>0')],
     [tree('10/r.id>0'), tree('l.id=r.id')],
     [tree('l.id=r.id'), tree('l.id>0')], []].each do |trees|
      options = { immediate: false, expanded: true }
      plain = join_tree_outcome(true, trees, nil, 0, **options)
      actual, calls = traced { join_tree_outcome(false, trees, nil, 0, **options) }
      mixed = trees.any? { |node| node[:left]&.dig(:kind) == :binary }
      assert(actual == plain && calls[:kernel].positive? == mixed,
        "mixed declined/compiled ON trees changed execution: #{[trees, actual, plain, calls].inspect}")
      assert(calls[:on_program_vectors] == (mixed ? 1 : 0) && calls[:on_program_kernels] == (mixed ? 1 : 0),
        'independent ON predicates allocated an unnecessary program vector or omitted a compiled entry')
      (1..plain[-2] + 1).each do |limit|
        [[limit, 0], [0, limit + 7]].each do |statement, lifetime|
          expected = join_tree_outcome(true, trees, nil, statement, **options, lifetime: lifetime)
          observed = join_tree_outcome(false, trees, nil, statement, **options, lifetime: lifetime)
          assert(observed == expected, 'mixed ON vector changed a statement/lifetime failure prefix')
        end
      end
    end
  end

  # The execution loops require these iterator operations, but admission must
  # not ask an arbitrary row source for its length or force its materialization.
  class UnsizedRows
    def initialize(rows) = (@rows = rows)
    def empty? = @rows.empty?
    def first = @rows.first
    def [](index) = @rows[index]
    def each_with_index(&block) = @rows.each_with_index(&block)
    def each_index(&block) = @rows.each_index(&block)
    def length = raise('join compiler inspected an unsized row source')
  end

  def candidate_gate
    threshold = JedNative::CompiledPlans::MIN_JOIN_PREDICATE_CANDIDATES
    simple_threshold = JedNative::CompiledPlans::MIN_SIMPLE_JOIN_PREDICATE_INPUTS
    [
      [0, threshold, false, {}], [1, 1, false, {}], [1, 2, false, {}],
      [1, threshold - 1, false, {}], [1, threshold, true, {}],
      [2, threshold / 2, true, {}],
      [threshold - 1, 1, false, { indexed: true }],
      [threshold, 1, true, { indexed: true }],
      [1, 1, true, { unsized_left: true }], [1, 1, true, { unsized_right: true }],
      [1, 1, true, { lateral: true }], [1, 1, true, { immediate: true }],
      [1, threshold, false, { simple: true }],
      [1, simple_threshold - 1, false, { simple: true }],
      [1, simple_threshold, true, { simple: true }],
      [simple_threshold - 1, 1, false, { simple: true, indexed: true }],
      [simple_threshold, 1, true, { simple: true, indexed: true }],
      [1, 1, true, { simple: true, unsized_left: true }],
      [1, 1, true, { simple: true, unsized_right: true }],
      [1, 1, true, { simple: true, indexed: true, unsized_left: true }],
      [1, 1, true, { simple: true, lateral: true, unsized_left: true, unsized_right: true }],
      [1, 1, true, { simple: true, immediate: true }],
      [1, 1, false, { immediate: true, disabled: :@native_compilation_disabled }],
      [1, 1, false, { immediate: true, disabled: :@native_join_compilation_disabled }]
    ].each do |left_size, right_size, admitted, options|
      current = engine
      current.instance_variable_set(:@native_compilation_immediate, options[:immediate])
      current.instance_variable_set(options[:disabled], true) if options[:disabled]
      left_rows = Array.new(left_size) { |index| [index + 1, true] }
      right_rows = Array.new(right_size) { |index| [index + 1, false] }
      left_rows = UnsizedRows.new(left_rows) if options[:unsized_left]
      right_rows = UnsizedRows.new(right_rows) if options[:unsized_right]
      left = JedNative::Relation.new(columns: COLUMNS.take(2), rows: left_rows)
      right = JedNative::Relation.new(columns: COLUMNS.drop(2), rows: right_rows)
      current.define_singleton_method(:source_relation) { |_| right }
      current.define_singleton_method(:join_index_relation) { |*| right }
      current.define_singleton_method(:lateral_source_relation) { |*| right }
      current.define_singleton_method(:lateral_source_dependency?) { |*| false }
      source = { kind: :table, name: 'r', native_join_algorithm: :nested }
      source[:native_index_plan] = { columns: right.columns } if options[:indexed]
      source[:lateral] = true if options[:lateral]
      on = tree(options[:simple] ? 'l.id<=r.id' : 'l.id+r.id>0')
      2.times { current.join(left, source, :inner, on, nil) }
      actual, counts = traced { current.join(left, source, :inner, on, nil) }
      cache = current.instance_variable_get(:@native_compiled_join_predicates) || {}
      expected = (1..left_size).flat_map do |left_id|
        (1..right_size).filter_map do |right_id|
          [left_id, true, right_id, false] unless options[:simple] && left_id > right_id
        end
      end
      assert(actual.rows == expected, 'candidate gate changed join execution')
      assert(cache.empty? != admitted && counts[:kernel].positive? == admitted,
        "candidate gate changed admission #{[left_size, right_size, options].inspect}: #{counts.inspect}")
    end
  end

  def setup(database)
    database.execute_script(<<~SQL)
      CREATE TABLE jp_l(id i32 PRIMARY KEY,k i32,label text,flag boolean);
      CREATE TABLE jp_r(id i32 PRIMARY KEY,k i32,label text,flag boolean);
      CREATE TABLE jp_h(id i32 PRIMARY KEY,k i32,label text,flag boolean);
      CREATE TABLE jp_empty(id i32 PRIMARY KEY,k i32);
      INSERT INTO jp_l VALUES(1,2,'a',TRUE),(2,5,'b',NULL),(3,9,'c',FALSE),(4,NULL,'d',TRUE),(5,4,'same',TRUE);
      INSERT INTO jp_r VALUES(2,2,'x',TRUE),(4,4,'same',NULL),(5,5,'b',FALSE),(6,2,'é',TRUE),(7,NULL,'null',TRUE);
      INSERT INTO jp_h VALUES(2,2,'x',TRUE),(4,4,'same',NULL),(5,5,'b',FALSE),(6,2,'é',TRUE),(7,NULL,'null',TRUE);
      CREATE INDEX jp_r_k ON jp_r(k);
    SQL
  end

  def session(database, disabled: false)
    database.session.tap do |current|
      current.engine.instance_variable_set(:@native_join_compilation_disabled, disabled)
      # Exercise the new dispatch on tiny fixtures; normal threshold and
      # second-use behavior are independently checked above.
      current.engine.instance_variable_set(:@native_compilation_immediate, true)
    end
  end

  def outcome(current, statement, sql, params, api, collect: false, take: nil)
    prior, phase, values, checkpoints = current.lifetime_cost, :admission, [], []
    result = api.to_s.end_with?('_prepared') ? current.public_send(api, statement, *params) : current.public_send(api, sql, *params)
    phase = :pull
    if result.is_a?(JedNative::Rows) && !collect
      result.each do |row|
        values << metadata(row.to_a)
        checkpoints << [result.cost, current.lifetime_cost - prior]
        break if take && values.length >= take
      end
    else
      values = metadata(result.rows)
    end
    [:rows, values, result.columns, result.column_types, checkpoints, result.cost, current.lifetime_cost - prior]
  rescue JedNative::Error => error
    [:error, phase, error.sqlstate, error.message, values, checkpoints, result&.cost || current.engine.cost, current.lifetime_cost - prior]
  ensure
    result&.close
  end

  def compare(database, sql, params = [], api: :query_prepared, setting: nil, limit: 0, collect: false, take: nil, positive: false, path: nil, callbacks: nil)
    observations = [true, false].map do |disabled|
      current = session(database, disabled: disabled)
      statement = current.prepare(sql)
      3.times { outcome(current, statement, sql, params, api) }
      current.public_send("#{setting}=", setting == :lifetime_max_cost ? current.lifetime_cost + limit : limit) if setting
      callbacks&.clear
      value, counts, kernels, scratch = traced { outcome(current, statement, sql, params, api, collect: collect, take: take) }
      value << callbacks.dup if callbacks
      assert(counts[:kernel].zero?, 'disabled join tier executed a cached kernel') if disabled
      assert(counts[:kernel].positive?, "join fixture did not execute a kernel: #{api} #{sql}") if positive && !disabled
      assert(counts[path].positive?, "join fixture missed #{path}: #{api} #{sql}") if path
      assert(scratch.all? { |file| file.instance_variable_get(:@file).nil? }, 'join drain/close/error retained a scratch file')
      [value, counts, kernels]
    ensure
      current&.close
    end
    assert(observations[0][0] == observations[1][0],
      "join predicate changed #{api} #{sql} #{setting}=#{limit}: #{observations.map(&:first).inspect}")
    observations.last
  end

  def public_paths(database, disk:)
    cases = [
      ['SELECT l.id,r.id FROM jp_l l JOIN jp_r r ON r.id=l.k ORDER BY l.id LIMIT 2', [], :join_index_relation],
      ['SELECT l.id,r.id FROM jp_l l LEFT JOIN jp_r r ON r.id=l.k AND l.label<>r.label ORDER BY l.id,r.id', [], nil],
      ['SELECT l.id,r.id FROM jp_l l LEFT JOIN jp_r r ON r.id=l.k WHERE r.id IS NULL ORDER BY l.id', [], nil],
      ['SELECT l.id,r.id FROM jp_l l JOIN jp_r r ON l.id+r.id>$1 ORDER BY l.id,r.id', [8], nil],
      ['SELECT l.id,r.id FROM jp_l l LEFT JOIN jp_h r ON l.k=r.k AND l.label<>r.label ORDER BY l.id,r.id', [], nil],
      ['SELECT l.id,r.id,c.id FROM jp_l l JOIN jp_r r ON l.k=r.k JOIN jp_r c ON c.id=r.id AND l.id+c.id>$1 ORDER BY l.id,r.id,c.id', [4], nil],
      ['SELECT l.id,r.id,c.id FROM jp_l l LEFT JOIN jp_r r ON l.k=r.k LEFT JOIN jp_r c ON c.id=r.id AND l.flag ORDER BY l.id,r.id,c.id', [], nil],
      ['SELECT l.id,r.id FROM jp_l l FULL JOIN jp_r r ON l.id+r.id=7 ORDER BY l.id,r.id', [], nil],
      ["SELECT l.id,r.label FROM jp_l l JOIN jp_r r ON l.label<r.label AND r.label LIKE 's%' ORDER BY l.id,r.id", [], nil],
      ['SELECT l.id,count(r.id) FROM jp_l l LEFT JOIN jp_r r ON r.id=l.k GROUP BY l.id ORDER BY l.id', [], disk ? :spill_index_join : nil],
      ['SELECT l.id,count(r.id) FROM jp_l l LEFT JOIN jp_h r ON l.k=r.k AND l.label<>r.label GROUP BY l.id ORDER BY l.id', [], disk ? :spill_join : nil]
    ]
    APIS.each do |api|
      cases.each { |sql, params, path| compare(database, sql, params, api: api, positive: true, path: path) }
      exact, = compare(database, *cases.first.take(2), api: api)
      assert(exact[1] == [[1, 2], [2, 5]] && exact[-2] == 14, 'index top-N fixture lost shared canonical cost14')
      compare(database, 'SELECT l.id,e.id FROM jp_l l LEFT JOIN jp_empty e ON l.id=e.id ORDER BY l.id', api: api)
    end
    if disk
      _, counts = compare(database, *cases.last.take(2), positive: true, path: :spill_join)
      assert(counts[:scratch].positive?, 'small-work_mem fixture did not spill')
    end
  end

  def public_guards(database)
    cases = [
      ['SELECT l.id,r.id FROM jp_l l JOIN jp_r r ON r.id=l.k ORDER BY l.id LIMIT 2', []],
      ['SELECT l.id,r.id FROM jp_l l LEFT JOIN jp_r r ON l.id=r.id AND 10/(r.id-4)>0 ORDER BY l.id', []]
    ]
    cases.each do |sql, params|
      complete, = compare(database, sql, params)
      APIS.each do |api|
        %i[max_cost lifetime_max_cost].each do |setting|
          (1..complete[-2] + 1).each { |limit| compare(database, sql, params, api: api, setting: setting, limit: limit) }
        end
      end
    end
    sql = 'SELECT l.id,count(r.id) FROM jp_l l LEFT JOIN jp_r r ON l.k=r.k AND l.label<>r.label GROUP BY l.id ORDER BY l.id'
    [1, 127, 512, 4096, 65_536].each do |limit|
      %i[query_prepared execute_prepared].each do |api|
        compare(database, sql, api: api, setting: :max_query_memory_bytes, limit: limit, collect: true)
      end
    end
    compare(database, sql, take: 1)
  end

  def fallback
    callbacks = []
    environment = JedNative::Environment.new.register_function(name: 'jp_probe', args: ['i32'], returns: 'i32', cost: 3) do |value|
      callbacks << value
      raise JedNative::Error.new('22023', 'join callback failed') if value == 7
      value
    end
    JedNative.memory(environment: environment) do |database|
      setup(database)
      APIS.each do |api|
        _, calls = compare(database, 'SELECT l.id,r.id FROM jp_l l JOIN jp_r r ON jp_probe(l.id+r.id)>0 ORDER BY l.id,r.id',
          api: api, callbacks: callbacks)
        assert(calls[:kernel].zero?, 'host ON expression partially compiled')
        compare(database, 'SELECT l.id,r.id FROM jp_l l JOIN jp_r r ON l.id+r.id>(SELECT max(id) FROM jp_l) ORDER BY l.id,r.id', api: api)
      end
    end
  end

  def snapshots(database)
    reader, writer = session(database), database.session
    sql = 'SELECT l.id,r.id FROM jp_l l LEFT JOIN jp_r r ON l.k=r.k AND l.id+r.id>$1 ORDER BY l.id,r.id'
    statement = reader.prepare(sql)
    3.times { reader.query_prepared(statement, 0).rows }
    before = reader.query_prepared(statement, 0).rows
    second = reader.query_prepared(statement, 8).rows
    cursor, pending = reader.query_prepared(statement, 0), reader.query_prepared(statement, 8)
    first = cursor.next.to_a
    writer.execute("UPDATE jp_r SET k=99 WHERE id=2")
    fresh = reader.query_prepared(statement, 0).rows
    assert(fresh != before && [first, *cursor.map(&:to_a)] == before && pending.rows == second,
      'join kernel captured stale rows/bindings or lost a cursor snapshot')
    admitted = reader.query_prepared(statement, 0)
    reader.revoke(:select, 'jp_r')
    denied = outcome(reader, statement, sql, [0], :query_prepared)
    assert(denied[0] == :error && denied[2] == '42501' && admitted.rows == fresh,
      'join kernel bypassed fresh privileges or changed captured capabilities')
    reader.reset_privileges
    captured = reader.query_prepared(statement, 0)
    writer.execute('DROP TABLE jp_r')
    writer.execute('CREATE TABLE jp_r(id i16 PRIMARY KEY,k i16,label text,flag boolean)')
    writer.execute("INSERT INTO jp_r VALUES(1,2,'new',TRUE)")
    assert(reader.query_prepared(statement, 0).rows != fresh && captured.rows == fresh, 'join kernel reused stale schema')
  ensure
    [cursor, pending, admitted, captured, reader, writer].each { |item| item&.close }
  end

  def detached_kernel
    current = engine(params: [+'value'], types: ['text'])
    current.instance_variable_set(:@native_compilation_immediate, true)
    node, = expression("l.label=$1", columns: [{ table: 'l', name: 'label', type: 'text' }], types: ['text'])
    row, params = [+'value'], current.instance_variable_get(:@params)
    kernel = current.native_compiled_join_predicate(node)
    kernel.call(current, row, params)
    [kernel, [current, node, row, params, params.first].map { |object| WeakRef.new(object) }]
  end

  def run
    scalar_semantics
    cache_admission
    simple_admission
    separate_on_trees
    candidate_gate
    [false, true].each do |disk|
      Dir.mktmpdir('jed-compiled-join-') do |directory|
        database = disk ? JedNative.create(File.join(directory, 'fixture.jed'), locking: :exclusive, work_mem: 128) : JedNative.memory
        setup(database)
        public_paths(database, disk: disk)
        public_guards(database)
        snapshots(database)
      ensure
        database&.close
      end
    end
    fallback
    kernel, references = Thread.new { detached_kernel }.value
    3.times { GC.start }
    assert(references.none?(&:weakref_alive?), 'join kernel retained engine, expression, row or bindings')
    assert(kernel.call(engine, ['value'], ['value']), 'detached join kernel lost usability')
    puts 'ruby-native compiled join predicates: ON control flow, costs, outer joins, spill paths, bounded cache and lifetime passed'
  end
end

NativeCompiledJoinPredicateTests.run
