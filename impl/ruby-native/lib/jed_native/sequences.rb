# frozen_string_literal: true

module JedNative
  module Sequences
    SERIAL_TYPES = { 'serial' => 'i32', 'serial4' => 'i32', 'bigserial' => 'i64',
                     'serial8' => 'i64', 'smallserial' => 'i16', 'serial2' => 'i16' }.freeze

    def snapshot = super + [(@sequence_session || {}).dup, @sequence_last]

    def restore(state)
      super(state.take(4))
      @sequence_session, @sequence_last = state[4]&.dup || {}, state[5]
    end

    def dispatch(stmt)
      if @read_only && !temporary_statement?(stmt) && !%i[select values set_operation setop with commit rollback rollback_to savepoint release].include?(stmt[:kind])
        raise Error.new('25006', 'read-only transaction')
      end
      case stmt[:kind]
      when :alter_sequence then alter_sequence(stmt)
      when :drop_sequence then drop_sequence(stmt)
      else
        result = super
        @read_only = !!stmt[:read_only] if stmt[:kind] == :begin
        @read_only = false if %i[commit rollback].include?(stmt[:kind])
        result
      end
    end

    def create_sequence(stmt)
      if relation_exists?(stmt[:name])
        return Result.new if stmt[:if_not_exists]
        raise Error.new('42P07', 'relation already exists')
      end
      @sequences[stmt[:name]] = sequence_definition(stmt[:options] || {})
      @dirty = true
      Result.new
    end

    def sequence_definition(options, old = nil)
      raise Error.new('0A000', 'sequence ownership options are not supported') if options.key?(:owned_by)
      type = Types.normalize(options[:type] || 'i64')
      bits = Types::INTEGER_BITS[type]
      raise Error.new('22023', 'sequence type must be an integer') unless bits
      increment = options.fetch(:increment, old ? old[:increment] : 1)
      raise Error.new('22023', 'zero sequence increment') if increment.zero?
      type_min, type_max = -(1 << (bits - 1)), (1 << (bits - 1)) - 1
      minimum = options.fetch(:minvalue, old && old[:min_value]) || (increment.positive? ? 1 : type_min)
      maximum = options.fetch(:maxvalue, old && old[:max_value]) || (increment.positive? ? type_max : -1)
      start = options.fetch(:start, old && old[:start]) || (increment.positive? ? minimum : maximum)
      cache = options.fetch(:cache, old ? old[:cache] : 1)
      unless minimum >= type_min && maximum <= type_max && minimum < maximum && start.between?(minimum, maximum) && cache.positive?
        raise Error.new('22023', 'invalid sequence bounds or start/cache')
      end
      definition = { min_value: minimum, max_value: maximum, increment: increment, start: start,
                     cache: cache, cycle: options.fetch(:cycle, old ? old[:cycle] : false),
                     value: old ? old[:value] : start, called: old ? old[:called] : false,
                     owned_by: old && old[:owned_by] }
      if options.key?(:restart)
        definition[:value] = options[:restart] == true ? start : options[:restart]
        definition[:called] = false
        raise Error.new('22023', 'restart value is outside bounds') unless definition[:value].between?(minimum, maximum)
      end
      raise Error.new('22023', 'sequence value is outside altered bounds') unless definition[:value].between?(minimum, maximum)
      definition
    end

    def alter_sequence(stmt)
      old = @sequences[stmt[:name]]
      return Result.new if !old && stmt[:if_exists]
      raise Error.new('42P01', 'sequence does not exist') unless old
      if stmt[:action] == :rename
        raise Error.new('42P07', 'relation already exists') if relation_exists?(stmt[:new_name])
        @sequences.delete(stmt[:name])
        @sequences[stmt[:new_name]] = old
        if old[:owned_by]
          table_name, ordinal = old[:owned_by]
          t = @tables[table_name]
          columns = t[:columns].map(&:dup)
          columns[ordinal][:default] = identity_default(stmt[:new_name])
          @tables[table_name] = t.merge(columns: columns)
        end
      else
        options = stmt[:options] || {}
        raise Error.new('42601', 'ALTER SEQUENCE needs an action') if options.empty?
        raise Error.new('0A000', 'changing sequence type is not supported') if options[:type]
        @sequences[stmt[:name]] = sequence_definition(options, old)
      end
      @dirty = true
      Result.new
    end

    def drop_sequence(stmt)
      (stmt[:names] || [stmt[:name]]).each do |name|
        sequence = @sequences[name]
        next if !sequence && stmt[:if_exists]
        raise Error.new('42P01', 'sequence does not exist') unless sequence
        raise Error.new(stmt[:cascade] ? '0A000' : '2BP01', 'owned sequence depends on its column') if sequence[:owned_by]
        @sequences.delete(name)
      end
      @dirty = true
      Result.new
    end

    def sequence_value(name, operation, value = nil, called = true)
      return nil if name.nil? || operation.to_s == 'setval' && (value.nil? || called.nil?)
      name = sequence_name(name)
      sequence = @sequences[name] || raise(Error.new('42P01', "sequence #{name} does not exist"))
      @sequence_session ||= {}
      case operation.to_s
      when 'nextval'
        raise Error.new('25006', 'read-only transaction') if @read_only
        charge
        value = sequence[:called] ? sequence[:value] + sequence[:increment] : sequence[:value]
        unless value.between?(sequence[:min_value], sequence[:max_value])
          raise Error.new('2200H', 'sequence generator limit exceeded') unless sequence[:cycle]
          value = sequence[:increment].positive? ? sequence[:min_value] : sequence[:max_value]
        end
        @sequences[name] = sequence.merge(value: value, called: true)
        @sequence_session[name] = value
        @sequence_last = name
        @dirty = true
        value
      when 'currval'
        raise Error.new('55000', 'sequence has not been used in this session') unless @sequence_session.key?(name)
        @sequence_session[name]
      when 'setval'
        raise Error.new('25006', 'read-only transaction') if @read_only
        charge
        raise Error.new('22003', 'sequence value is outside bounds') unless value.between?(sequence[:min_value], sequence[:max_value])
        @sequences[name] = sequence.merge(value: value, called: called)
        @sequence_session[name] = value if called
        @dirty = true
        value
      end
    end

    def sequence_name(value)
      text = value.strip
      if text.start_with?('"')
        raise Error.new('42602', 'invalid sequence name') unless text.match?(/\A"(?:[^"]|"")*"\z/)
        text[1...-1].gsub('""', '"')
      else
        raise Error.new('42602', 'invalid sequence name') unless text.match?(/\A[^\s."]+\z/)
        text.downcase
      end
    end

    def create_table(stmt)
      if @sequences.key?(stmt[:name])
        return Result.new if stmt[:if_not_exists]
        raise Error.new('42P07', 'relation already exists')
      end
      return super if stmt[:query] || relation_exists?(stmt[:name])
      identities = {}
      columns = stmt[:columns].each_with_index.map do |column, ordinal|
        serial_type = SERIAL_TYPES[column[:type]]
        identity = column[:identity]
        next column unless serial_type || identity
        raise Error.new('42601', 'conflicting serial, identity, or default') if column[:default] || serial_type && identity
        type = serial_type || Types.normalize(column[:type])
        raise Error.new('22023', 'identity requires an integer column') unless Types::INTEGER_BITS.key?(type)
        name = create_owned_sequence({ name: stmt[:name] }, column.merge(type: type), ordinal,
                                     identity ? identity[:options] : {})
        identities[ordinal] = identity[:mode] if identity
        column.reject { |key, _| key == :identity }.merge(type: type, nullable: false,
          default: identity_default(name))
      end
      result = super(stmt.merge(columns: columns))
      unless identities.empty?
        t = @tables[stmt[:name]]
        @tables[stmt[:name]] = t.merge(columns: t[:columns].each_with_index.map { |c, i| identities[i] ? c.merge(identity: identities[i]) : c })
      end
      result
    end

    def drop_table(stmt)
      result = super
      names = stmt[:names] || [stmt[:name]]
      @sequences = @sequences.reject { |_name, sequence| sequence[:owned_by] && names.include?(sequence[:owned_by][0]) }
      result
    end

    def relation_exists?(name)
      @tables.key?(name) || @views.key?(name) || @sequences.key?(name) ||
        @tables.values.any? { |table| table[:indexes].any? { |index| index[:name] == name } }
    end
  end
  Engine.prepend(Sequences)
end
