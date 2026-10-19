# frozen_string_literal: true

require_relative 'compiled_text_test'

# The corpus owns SQL semantics. These probes cover transient text snapshots
# and labels at the internal boundary where a comparison consumes its operands.
module NativeCompiledTextElisionTests
  module_function

  Helpers = NativeCompiledTextTests
  COLUMNS = [{ name: 'a', type: 'text' }, { name: 'b', type: 'text' },
             { name: 'id', type: 'i32' }].freeze

  def assert(value, message) = Helpers.assert(value, message)

  def canonical_image(value)
    case value
    when String then String.new(value)
    when Array then value.map { |item| canonical_image(item) }
    when Hash then value.transform_values { |item| canonical_image(item) }
    else value
    end
  end

  def image(value) = canonical_image(Helpers.metadata(value))

  class ObservedRow
    attr_reader :values, :ordinal

    def initialize(values, ordinal, events, on_read = nil)
      @values, @ordinal, @events, @on_read = values, ordinal, events, on_read
    end

    def [](index)
      @events << [:read, @ordinal, index]
      @on_read&.call(index)
      @values[index]
    end

    def length = @values.length

    def each_with_index
      length.times { |index| yield self[index], index }
    end
  end

  module EngineProbe
    attr_accessor :elision_events, :elision_charge_hook, :elision_observe_work

    def charge(amount = 1)
      elision_events << [:charge, amount, cost]
      elision_charge_hook&.call(amount, cost)
      super
    end

    def cast_value(value, type, source_type: nil)
      elision_events << [:cast, NativeCompiledTextElisionTests.image(value), type, source_type] if type == 'text'
      super
    end

    def scalar_work(operator, left, right)
      if elision_observe_work
        elision_events << [:work, operator, NativeCompiledTextElisionTests.image(left), NativeCompiledTextElisionTests.image(right)]
      end
      super
    end

    def query_memory_row_bytes(row, columns = nil, mask = nil)
      elision_events << [:row_bytes, row.ordinal]
      super
    end

    def query_memory_release(owner, bytes = nil)
      elision_events << [:release, owner.nil?, bytes]
      super
    end
  end

  def resolve(sql, types: [], filter: false)
    Helpers.resolve(sql, types: types, columns: COLUMNS, filter: filter)
  end

  def build(mode, expressions, types)
    case mode
    when :scalar then JedNative::CompiledExpression.build(expressions.first, types: types)
    when :projection then JedNative::CompiledExpression.build_many(expressions, types: types)
    when :arguments then JedNative::CompiledExpression.prepare_many(expressions, types: types)&.materialize
    when :keys then JedNative::CompiledRelation.prepare_keys(expressions, types: types)&.materialize
    when :filter, :memory_filter
      JedNative::CompiledRelation.prepare_filter(expressions.first, types: types, memory: mode == :memory_filter)&.materialize
    when :array, :stream
      JedNative::CompiledPipeline.build(nil, expressions, types: types, mode: mode)
    when :aggregate then JedNative::CompiledAggregate.build(expressions.first, types: types)
    end
  end

  def ordinary(current, mode, expressions, rows, owner)
    case mode
    when :scalar then current.evaluate(expressions.first, rows.first)
    when :projection, :arguments then expressions.map { |expression| current.evaluate(expression, rows.first) }
    when :keys then rows.map { |row| expressions.map { |expression| current.evaluate(expression, row) } }
    when :filter, :memory_filter
      rows.select do |row|
        accepted = current.evaluate(expressions.first, row) == true
        current.query_memory_release(owner, current.query_memory_row_bytes(row, COLUMNS, [true, true, true])) if mode == :memory_filter && !accepted
        accepted
      end
    when :array, :stream
      rows.map do |row|
        current.charge if mode == :stream
        values = expressions.map { |expression| current.evaluate(expression, row) }
        current.charge if mode == :stream
        values
      end
    when :aggregate then current.compute_aggregate(expressions.first, rows)
    end
  end

  def outcome(mode, expressions, kernel, factory, types, statement, lifetime, prior)
    events = []
    rows, params, hook, observe_work = factory.call(events)
    current = Helpers.engine(params, types)
    current.singleton_class.prepend(EngineProbe)
    current.elision_events, current.elision_charge_hook = events, hook
    current.elision_observe_work = observe_work
    current.max_cost, current.lifetime_max_cost = statement, lifetime
    current.instance_variable_set(:@lifetime_cost, prior)
    account = JedNative::QueryMemoryAccount.new(8192)
    account.reserve(4096)
    owner = nil
    if mode == :memory_filter
      current.instance_variable_set(:@query_memory, account)
      owner = current.query_memory_owner
      owner.bytes = 4096
    end
    begin
      result = if !kernel
        ordinary(current, mode, expressions, rows, owner)
      elsif %i[array stream].include?(mode)
        kernel.call(current, rows, params, [])
      elsif %i[scalar projection arguments].include?(mode)
        kernel.call(current, rows.first, params)
      else
        kernel.call(current, rows, params, *([owner, COLUMNS, [true, true, true]] if mode == :memory_filter))
      end
      result = result.map(&:ordinal) if %i[filter memory_filter].include?(mode)
      answer = [:value, image(result)]
    rescue JedNative::Error => error
      answer = [:error, error.sqlstate, error.message]
    end
    [*answer, current.cost, current.lifetime_cost, events, account.used, owner&.bytes]
  end

  def compare(sql, mode: :scalar, types: [], expressions: nil, expected: nil, &factory)
    expression, registry = resolve(sql, types: types, filter: %i[filter memory_filter].include?(mode))
    expressions ||= [expression]
    kernel = build(mode, expressions, registry)
    assert(kernel, "text elision fixture was not admitted: #{mode} #{sql}")
    baseline = outcome(mode, expressions, nil, factory, types, 0, 0, 0)
    result = outcome(mode, expressions, kernel, factory, types, 0, 0, 0)
    assert(result == baseline, "text elision changed #{mode} #{sql}: #{result.inspect} != #{baseline.inspect}")
    assert(baseline[0, expected.length] == expected, "unexpected fixture result: #{baseline.inspect}") if expected
    (1..baseline[-5] + 1).each do |limit|
      [[limit, 0, 0], [0, 11 + limit, 11], [limit, 11 + limit, 11]].each do |statement, lifetime, prior|
        reference = outcome(mode, expressions, nil, factory, types, statement, lifetime, prior)
        actual = outcome(mode, expressions, kernel, factory, types, statement, lifetime, prior)
        assert(actual == reference, "text elision changed #{mode} #{sql} at #{[statement, lifetime, prior].inspect}: #{actual.inspect} != #{reference.inspect}")
      end
    end
    result
  end

  def mutable_snapshots
    %w[= <> != < <= > >=].each do |operator|
      compare("SELECT a #{operator} b") do |events|
        left = +'alpha'
        row = ObservedRow.new([left, 'beta', 1], 0, events, ->(index) { left.replace('zzzzzzzz') if index == 1 })
        [[row], []]
      end
      compare("SELECT $1 #{operator} b", types: ['text']) do |events|
        left = +'alpha'
        row = ObservedRow.new(['unused', 'beta', 1], 0, events, ->(index) { left.replace('zzzzzzzz') if index == 1 })
        [[row], [left]]
      end
    end
    compare('SELECT a < b', expected: [:value, true]) do |events|
      left, right = +'alpha', +'beta'
      hook = lambda do |amount, _cost|
        if amount > 1
          left.replace('zzzzzz')
          right.replace('aaaaaa')
        end
      end
      [[ObservedRow.new([left, right, 1], 0, events)], [], hook]
    end
    # Casts are observable normalization boundaries; their input must retain
    # the same wrapper metadata even when the enclosing result is boolean.
    ['SELECT a::text < b', 'SELECT a < b::text', 'SELECT (a::text)::text < b', 'SELECT $1::text < b'].each do |sql|
      types = sql.include?('$1') ? ['text'] : []
      compare(sql, types: types) do |events|
        value = +'alpha'
        row = ObservedRow.new([value, 'beta', 1], 0, events, ->(index) { value.replace('zzzzzz') if index == 1 })
        [[row], types.empty? ? [] : [value]]
      end
    end
  end

  def custom_string(value, frozen: false)
    result = String.new(value)
    def result.length = super + 7
    def result.<=>(other) = -(super)
    frozen ? result.freeze : result
  end

  def class_sensitive_string(value)
    result = String.new(value)
    def result.<=>(other) = other.instance_of?(JedNative::CollatedText) ? super : -(super)
    def result.==(other) = other.instance_of?(JedNative::CollatedText)
    result
  end

  def custom_collated_text(value, label, frozen: false)
    result = JedNative::CollatedText.new(value, label)
    def result.length = super + 7
    def result.<=>(other) = -(super)
    frozen ? result.freeze : result
  end

  def collated_snapshot_inputs
    labels = [
      JedNative::CollationLabel.new(name: 'unicode', table: Helpers.unicode_environment.collations.fetch('unicode')).freeze,
      JedNative::CollationLabel.new(conflict: true).freeze
    ]
    labels.each do |label|
      [false, true].each do |frozen|
        %w[= <> != < <= > >=].each do |operator|
          ["SELECT a #{operator} b", "SELECT b #{operator} a"].each do |sql|
            compare(sql) do |events|
              value = custom_collated_text('z', label, frozen: frozen)
              [[ObservedRow.new([value, 'ä', 1], 0, events)], []]
            end
          end
        end
        compare('SELECT a') do |events|
          [[ObservedRow.new([custom_collated_text('z', label, frozen: frozen), 'ä', 1], 0, events)], []]
        end
      end
      compare('SELECT a < b', expected: [:value, true]) do |events|
        left = custom_collated_text('alpha', label)
        row = ObservedRow.new([left, 'beta', 1], 0, events, ->(index) { left.replace('zzzzzzzz') if index == 1 })
        [[row], []]
      end
      compare('SELECT a < b', expected: [:value, true]) do |events|
        left, right = custom_collated_text('alpha', label), custom_collated_text('beta', label)
        hook = lambda do |amount, _cost|
          if amount > 1
            left.replace('zzzzzz')
            right.replace('aaaaaa')
          end
        end
        [[ObservedRow.new([left, right, 1], 0, events)], [], hook]
      end
    end
  end

  def runtime_classes_and_labels
    labels = [JedNative::CollationLabel.new(name: 'C').freeze,
      JedNative::CollationLabel.new(name: 'unicode', table: Helpers.unicode_environment.collations.fetch('unicode')).freeze,
      JedNative::CollationLabel.new(conflict: true).freeze,
      JedNative::CollationLabel.new(none: true).freeze]
    makers = [-> { +'z' }, -> { 'z'.freeze }, -> { custom_string('z') }, -> { custom_string('z', frozen: true) },
      -> { class_sensitive_string('z') },
      -> { Helpers::HostText.new('z') }, -> { JedNative::Bytea.new('z') }, -> { JedNative::UUID.new('z') }]
    labels.each { |label| makers << -> { JedNative::CollatedText.new('z', label) } }
    %w[= <> != < <= > >=].each do |operator|
      makers.each do |make|
        ["SELECT a #{operator} b", "SELECT b #{operator} a", "SELECT a #{operator} 'ä'", "SELECT 'ä' #{operator} a"].each do |sql|
          compare(sql) { |events| [[ObservedRow.new([make.call, 'ä', 1], 0, events)], []] }
        end
        ["SELECT $1 #{operator} b", "SELECT b #{operator} $1"].each do |sql|
          compare(sql, types: ['text']) { |events| [[ObservedRow.new(['unused', 'ä', 1], 0, events)], [make.call]] }
        end
      end
    end
  end

  def eager_errors
    %w[= <> != < <= > >=].each do |operator|
      compare("SELECT a #{operator} b", expected: [:error, 'XX001']) do |events|
        fail_read = ->(index) { raise JedNative::Error.new('XX001', 'right operand failed') if index == 1 }
        [[ObservedRow.new([nil, 'unused', 1], 0, events, fail_read)], []]
      end
    end
    [['SELECT a IS NOT NULL AND a < b', nil], ["SELECT a = 'stop' OR a < b", 'stop']].each do |sql, left|
      compare(sql) do |events|
        fail_read = ->(index) { raise JedNative::Error.new('XX001', 'lazy operand read') if index == 1 }
        [[ObservedRow.new([left, 'unused', 1], 0, events, fail_read)], []]
      end
    end
    ['SELECT 1 WHERE a < b', 'SELECT 1 WHERE a < b AND id > 0'].each do |sql|
      %i[filter memory_filter].each do |mode|
        compare(sql, mode: mode) do |events|
          values = [[nil, 'beta', 1], ['alpha', 'beta', 2], ['z', 'beta', 3]]
          [values.each_with_index.map { |row, index| ObservedRow.new(row, index, events) }, []]
        end
      end
    end
  end

  def static_parameters_and_fallback_labels
    expression, = resolve('SELECT a < b')
    label = expression.args.first.instance_variable_get(:@native_collation)
    [false, true].each do |contextual|
      parameter = JedNative::Expression.new(kind: :param, type: 'text', value: 0, extra: { context_type: contextual })
      parameter.instance_variable_set(:@native_collation, label)
      expression.args = [parameter, expression.args.last]
      compare('SELECT a < b', expressions: [expression], types: ['text']) do |events|
        left = +'alpha'
        row = ObservedRow.new(['unused', 'beta', 1], 0, events, ->(index) { left.replace('zzzzzz') if index == 1 })
        [[row], [left]]
      end
    end

    # Unexpected runtime types take the helper. Observe its actual operands to
    # prove it restores each child's own flags, rather than the parent's label.
    %w[= <].each do |operator|
      expression, = resolve("SELECT a #{operator} b")
      expression.args.first.instance_variable_set(:@native_collation,
        JedNative::CollationLabel.new(name: 'C', explicit: true, default: nil, conflict: false, none: nil).freeze)
      expression.args.last.instance_variable_set(:@native_collation,
        JedNative::CollationLabel.new(name: 'C', explicit: nil, default: true, conflict: nil, none: false).freeze)
      [JedNative::Bytea, JedNative::UUID].each do |klass|
        [false, true].each do |reverse|
          compare("SELECT a #{operator} b", expressions: [expression]) do |events|
            values = [+'alpha', klass.new('beta')]
            values.reverse! if reverse
            [[ObservedRow.new([*values, 1], 0, events)], [], nil, true]
          end
        end
      end
    end
  end

  def admission
    ["SELECT a < b", "SELECT a >= 'constant'", "SELECT 'constant' = b", "SELECT 'a' <> 'b'"].each do |sql|
      expression, registry = resolve(sql)
      kernel = JedNative::CompiledExpression.build(expression, types: registry)
      assert(kernel&.source&.include?('# C text snapshots'), "eligible comparison did not elide wrappers: #{sql}")
    end
    ['SELECT $1 < b', 'SELECT a < $1', 'SELECT $1 < $2', 'SELECT a::text < b', 'SELECT a < b::text'].each do |sql|
      types = Array.new(sql.include?('$2') ? 2 : sql.include?('$1') ? 1 : 0, 'text')
      expression, registry = resolve(sql, types: types)
      kernel = JedNative::CompiledExpression.build(expression, types: registry)
      assert(kernel && !kernel.source.include?('# C text snapshots'), "unproven comparison elided wrappers: #{sql}")
    end
    expression, registry = resolve('SELECT a < b')
    %i[projection arguments keys array stream filter memory_filter].each do |mode|
      kernel = build(mode, [expression], registry)
      assert(kernel&.source&.include?('# C text snapshots'), "#{mode} did not admit text snapshots")
    end
    aggregate, registry = resolve('SELECT count(*) FILTER (WHERE a < b)')
    assert(build(:aggregate, [aggregate], registry)&.source&.include?('# C text snapshots'), 'aggregate FILTER did not admit text snapshots')
    ['SELECT a', 'SELECT a::text', "SELECT 'constant'"].each do |sql|
      expression, registry = resolve(sql)
      kernel = JedNative::CompiledExpression.build(expression, types: registry)
      value = kernel.call(Helpers.engine, ['alpha'], [])
      assert(value.instance_of?(JedNative::CollatedText) && value.collation_label.name == 'C', 'escaping scalar text lost its C wrapper')
    end
  end

  def escaping_values_and_consumers
    comparison, = resolve('SELECT a < b')
    text, = resolve('SELECT a')
    cast, = resolve('SELECT a::text')
    expressions = [comparison, text, cast]
    %i[projection arguments keys array stream].each do |mode|
      result = compare('SELECT a < b', mode: mode, expressions: expressions) do |events|
        values = [[+'alpha', 'beta', 1], [nil, 'beta', 2], ['é🙂', 'ê🙂', 3]]
        [values.each_with_index.map { |row, index| ObservedRow.new(row, index, events) }, []]
      end
      first = %i[projection arguments].include?(mode) ? result[1] : result[1].first
      assert(first[1][0] == 'JedNative::CollatedText' && first[2][0] == 'JedNative::CollatedText',
        "escaping #{mode} text lost its wrapper")
    end
    %i[filter memory_filter].each do |mode|
      compare('SELECT 1 WHERE a < b', mode: mode) do |events|
        left = +'alpha'
        row = ObservedRow.new([left, 'beta', 1], 0, events, ->(index) { left.replace('zzzzzz') if index == 1 })
        [[row], []]
      end
    end
    ['SELECT count(*) FILTER (WHERE a < b)', 'SELECT sum(id) FILTER (WHERE a < b)'].each do |sql|
      compare(sql, mode: :aggregate) do |events|
        left = +'alpha'
        row = ObservedRow.new([left, 'beta', 1], 0, events, ->(index) { left.replace('zzzzzz') if index == 1 })
        [[row], []]
      end
    end
  end

  def run
    admission
    mutable_snapshots
    collated_snapshot_inputs
    runtime_classes_and_labels
    eager_errors
    static_parameters_and_fallback_labels
    escaping_values_and_consumers
    puts 'ruby-native compiled text elision: operand snapshots, custom strings, dynamic labels, eager faults, exact guards and escaping wrappers passed'
  end
end

NativeCompiledTextElisionTests.run
