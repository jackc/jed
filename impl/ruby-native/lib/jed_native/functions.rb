# frozen_string_literal: true

require_relative 'types'
require_relative 'json_value'
require_relative 'decimal_math'
require_relative 'function_catalog'
require 'openssl'

module JedNative
  module Functions
    AGGREGATES = %w[count count_star sum avg min max bool_and bool_or every bit_and bit_or bit_xor array_agg string_agg json_agg jsonb_agg json_agg_strict jsonb_agg_strict variance var_samp var_pop stddev stddev_samp stddev_pop corr covar_pop covar_samp regr_avgx regr_avgy regr_count regr_intercept regr_r2 regr_slope regr_sxx regr_sxy regr_syy].freeze
    WINDOWS = %w[row_number rank dense_rank percent_rank cume_dist ntile lag lead first_value last_value nth_value].freeze
    NULL_TOLERANT = %w[coalesce nullif greatest least concat concat_ws num_nulls num_nonnulls array_append array_prepend array_cat array_remove array_replace array_position array_positions array_to_string string_to_array json_build_array jsonb_build_array json_build_object jsonb_build_object json_scalar to_json to_jsonb array_to_json row_to_json i32range i64range int4range int8range numrange tsrange tstzrange daterange format quote_nullable current_setting].freeze
    INTEGER_RESULTS = %w[length char_length character_length octet_length bit_length ascii strpos position cardinality array_ndims array_length array_lower array_upper array_position num_nulls num_nonnulls scale min_scale get_bit get_byte uuid_extract_version count count_star regr_count row_number rank dense_rank ntile].freeze
    TEXT_RESULTS = %w[lower upper initcap substr substring left right replace translate reverse repeat lpad rpad trim btrim ltrim rtrim split_part concat concat_ws array_to_string format to_char quote_ident quote_literal quote_nullable encode to_hex md5 json_typeof jsonb_typeof jsonb_pretty current_setting chr overlay].freeze
    FLOAT_RESULTS = %w[pi degrees radians acos asin atan atan2 acosh asinh atanh cos sin tan cot cosh sinh tanh cbrt corr covar_pop covar_samp regr_avgx regr_avgy regr_intercept regr_r2 regr_slope regr_sxx regr_sxy regr_syy date_part percent_rank cume_dist].freeze
    BOOLEAN_RESULTS = %w[starts_with like ilike like_escape isempty lower_inc upper_inc lower_inf upper_inf bool_and bool_or every jsonb_path_exists jsonb_path_match].freeze
    ESCAPE_BYTES = Array.new(256) do |byte|
      (byte == 92 ? '\\\\' : byte < 32 || byte > 126 ? Kernel.format('\\%03o', byte) : byte.chr).freeze
    end.freeze
    module_function
    def aggregate?(name) = AGGREGATES.include?(name.to_s.downcase)
    def return_type(name, arg_types)
      name = name.to_s.downcase
      arg_types = arg_types.map { |t| Types.normalize(t) }
      return 'decimal' if name == 'extract'
      return 'text[]' if name == 'regexp_matches'
      return 'text' if name == 'regexp_split_to_table'
      return JsonPathFunctions.result_type(name) if defined?(JsonPathFunctions) && JsonPathFunctions.function?(name)
      signature = resolve_signature(name, arg_types)
      if signature && !%w[promoted anycompatiblearray anyelement anyrange].include?(signature[1])
        return signature[1]
      end
      if signature && signature[1] == 'anycompatiblearray'
        elements = arg_types.reject { |t| t == 'unknown' }.map { |t| t.sub(/\[\]\z/, '') }
        begin
          return elements.reduce('unknown') { |a, b| Types.promote(a, b) } + '[]'
        rescue Error
          raise Error.new('42883', "function #{name} has incompatible array elements")
        end
      end
      first = arg_types.first || 'unknown'
      return first == 'bytea' ? 'bytea' : 'text' if %w[substr substring overlay].include?(name)
      return Types::RANGES.fetch(first, 'text') if %w[lower upper].include?(name) && Types::RANGES.key?(first)
      return 'i64' if INTEGER_RESULTS.include?(name)
      return 'text' if TEXT_RESULTS.include?(name)
      return 'f64' if FLOAT_RESULTS.include?(name)
      return 'boolean' if BOOLEAN_RESULTS.include?(name)
      return 'bytea' if %w[decode sha224 sha256 sha384 sha512 set_bit set_byte].include?(name)
      return 'uuid' if %w[uuidv4 uuidv7 gen_random_uuid].include?(name)
      return 'timestamptz' if %w[now current_timestamp transaction_timestamp statement_timestamp clock_timestamp make_timestamptz uuid_extract_timestamp].include?(name)
      return 'timestamp' if name == 'make_timestamp'
      return 'date' if %w[current_date make_date to_date].include?(name)
      return 'interval' if %w[age make_interval justify_days justify_hours justify_interval].include?(name)
      return 'text[]' if name == 'string_to_array'
      return 'i32[]' if name == 'array_positions'
      return arg_types.last if name == 'array_prepend'
      return first + '[]' if name == 'array_agg'
      return first.sub(/\[\]\z/, '') if name == 'unnest'
      return 'decimal' if %w[factorial].include?(name)
      return first.start_with?('f') ? first : 'decimal' if %w[avg variance var_pop var_samp stddev stddev_pop stddev_samp].include?(name)
      return first == 'i64' ? 'decimal' : first.start_with?('i') ? 'i64' : first if name == 'sum'
      return 'jsonb' if name.start_with?('jsonb_') && !name.end_with?('_text')
      return 'json' if name.start_with?('json_') || %w[to_json array_to_json row_to_json].include?(name)
      return 'jsonb' if name == 'to_jsonb'
      return Types.normalize(name) if Types::RANGES.key?(Types.normalize(name))
      return arg_types.reduce('unknown') { |a, b| Types.promote(a, b) } if %w[coalesce greatest least].include?(name)
      first
    end
    def family(type)
      type = Types.base(type)
      return 'integer' if Types::INTEGER_BITS.key?(type)
      return 'float' if %w[f32 f64].include?(type)
      type
    end
    def resolve_signature(name, types)
      signatures = FunctionCatalog::SIGNATURES[name]
      return nil unless signatures
      match = signatures.find do |families, _result, promote, variadic, defaults|
        next false unless variadic ? types.length >= families.length - 1 : types.length.between?(families.length - defaults, families.length)
        types.each_with_index.all? do |type, i|
          expected = families[[i, families.length - 1].min]
          actual = family(type)
          next true if actual == 'unknown' || expected == 'any' || expected == 'anycompatible'
          case expected
          when 'anyarray', 'anycompatiblearray' then type.end_with?('[]')
          when 'anyrange' then Types::RANGES.key?(type)
          else actual == expected || (promote && expected == 'decimal' && actual == 'integer' && types.any? { |t| family(t) == 'decimal' })
          end
        end
      end
      raise Error.new('42883', "function #{name}(#{types.join(', ')}) does not exist") unless match
      match
    end
    def call(name, args, context: nil, arg_types: nil)
      name = name.to_s.downcase
      resolve_signature(name, arg_types || args.map { |v| Types.infer(v) })
      return nil if !NULL_TOLERANT.include?(name) && args.any?(&:nil?)
      a, b, c, d = args
      case name
      when 'coalesce' then args.find { |v| !v.nil? }
      when 'nullif' then !a.nil? && !b.nil? && Types.compare(a, b).zero? ? nil : a
      when 'greatest' then args.compact.max { |x, y| Types.compare(x, y) }
      when 'least' then args.compact.min { |x, y| Types.compare(x, y) }
      when 'num_nulls' then args.count(nil)
      when 'num_nonnulls' then args.length - args.count(nil)
      when 'abs' then Types::INTEGER_BITS.key?(arg_types&.first) ? Types.check_integer(a.abs, arg_types.first) : a.abs
      when 'sign'
        raise Error.new('42883', 'sign does not accept integer arguments') if a.is_a?(Integer)
        a.is_a?(Decimal) ? Decimal.new(a <=> 0) : a.nan? ? 0.0 : (a <=> 0).to_f
      when 'round' then a.is_a?(Decimal) || a.is_a?(Integer) ? Decimal.parse(a).round(b || 0) : a.round(b || 0, half: :up).to_f
      when 'trunc' then a.is_a?(Decimal) ? a.trunc(b || 0) : a.truncate(b || 0).to_f
      when 'ceil', 'ceiling' then a.is_a?(Decimal) ? a.ceil : a.ceil.to_f
      when 'floor' then a.is_a?(Decimal) ? a.floor : a.floor.to_f
      when 'mod' then Types.binary('%', a, b)
      when 'div' then (Decimal.parse(a) / Decimal.parse(b)).trunc
      when 'scale' then Decimal.parse(a).scale
      when 'min_scale' then Decimal.parse(a).trim_scale.scale
      when 'trim_scale' then Decimal.parse(a).trim_scale
      when 'gcd', 'lcm'
        if a.is_a?(Decimal) || b.is_a?(Decimal)
          a, b = Decimal.parse(a), Decimal.parse(b)
          scale = [a.scale, b.scale].max
          x, y = a.round(scale).coefficient, b.round(scale).coefficient
          Decimal.new(name == 'gcd' ? x.gcd(y) : x.lcm(y), scale)
        else
          Types.check_integer(name == 'gcd' ? a.gcd(b) : a.lcm(b), (arg_types || []).first || 'i64')
        end
      when 'width_bucket'
        count = d
        raise Error.new('2201G', 'invalid argument for width_bucket') unless count.is_a?(Integer) && count.positive? && Types.compare(b, c) != 0 && ![a, b, c].any? { |v| v.is_a?(Float) && v.nan? }
        descending = Types.compare(b, c).positive?
        return 0 if descending ? Types.compare(a, b).positive? : Types.compare(a, b).negative?
        return count + 1 if descending ? Types.compare(a, c) <= 0 : Types.compare(a, c) >= 0
        if a.is_a?(Float)
          (((a - b) / (c - b)) * count).floor + 1
        else
          ratio = (Decimal.parse(a).to_r - Decimal.parse(b).to_r) / (Decimal.parse(c).to_r - Decimal.parse(b).to_r)
          (ratio * count).floor + 1
        end
      when 'factorial'
        raise Error.new('22003', 'factorial of a negative number is undefined') if a.negative?
        raise Error.new('54000', 'factorial argument too large') if a > 32_000
        product = 1
        2.upto(a) { |i| context&.charge(((product.to_s.length + 3) / 4) * ((i.to_s.length + 3) / 4) - 1); product *= i }
        Decimal.new(product)
      when 'pi' then Math::PI
      when 'degrees' then a.to_f * (180.0 / Math::PI)
      when 'radians' then a.to_f * (Math::PI / 180.0)
      when 'sqrt', 'exp', 'ln', 'log', 'log10', 'pow', 'power'
        math(name, args, context: context)
      when 'sin', 'cos', 'tan', 'asin', 'acos', 'atan', 'atan2', 'sinh', 'cosh', 'tanh', 'asinh', 'acosh', 'atanh', 'cbrt'
        Math.public_send(name, *args.map(&:to_f))
      when 'cot' then 1.0 / Math.tan(a.to_f)
      when 'length', 'char_length', 'character_length' then a.length
      when 'octet_length' then a.bytesize
      when 'bit_length' then a.bytesize * 8
      when 'ascii' then a.empty? ? 0 : a.ord
      when 'chr'
        raise Error.new('54000', 'null character not permitted') if a.zero?
        raise Error.new('22023', 'requested character too small') if a.negative?
        raise Error.new('54000', 'requested character too large') if a > 0x10ffff || (0xd800..0xdfff).cover?(a)
        a.chr(Encoding::UTF_8)
      when 'lower' then a.is_a?(RangeValue) ? a.lower : a.tr('A-Z', 'a-z')
      when 'upper' then a.is_a?(RangeValue) ? a.upper : a.tr('a-z', 'A-Z')
      when 'initcap'
        beginning = true
        a.each_char.map do |char|
          if /[A-Za-z0-9]/.match?(char)
            next_char = beginning ? char.tr('a-z', 'A-Z') : char.tr('A-Z', 'a-z')
            beginning = false
            next_char
          else beginning = true; char
          end
        end.join
      when 'substr', 'substring'
        raise Error.new('0A000', 'regular expression substring is not implemented in Ruby experiment') if b.is_a?(String)
        raise Error.new('22011', 'negative substring length not allowed') if c && c.negative?
        start = [b, 1].max - 1
        length = c ? [b + c - 1 - start, 0].max : a.length
        result = a[start, length] || a[0, 0]
        a.is_a?(Bytea) ? Bytea.new(result) : result
      when 'left' then a[0, b.negative? ? [a.length + b, 0].max : b] || ''
      when 'right'
        n = b.negative? ? [a.length + b, 0].max : [b, a.length].min
        n.zero? ? '' : a[-n, n]
      when 'strpos', 'position' then (a.index(b) || -1) + 1
      when 'starts_with' then a.start_with?(b)
      when 'reverse' then a.reverse
      when 'repeat'
        count = [b, 0].max
        context&.charge(a.bytesize)
        reserve(context, a.bytesize * count)
        a * count
      when 'replace'
        return a if b.empty?
        matches = a.scan(b).length
        reserve(context, a.bytesize + a.bytesize + matches * (c.bytesize - b.bytesize))
        a.gsub(b, c)
      when 'translate'
        mapping = {}
        source, target = b.chars, c.chars
        source.each_with_index { |char, i| mapping[char] = target[i] unless mapping.key?(char) }
        a.each_char.map { |char| mapping.key?(char) ? mapping[char].to_s : char }.join
      when 'ltrim', 'rtrim', 'btrim', 'trim'
        chars = (b || ' ').chars
        start, finish = 0, a.length
        start += 1 while name != 'rtrim' && start < finish && chars.include?(a[start])
        finish -= 1 while name != 'ltrim' && finish > start && chars.include?(a[finish - 1])
        result = a[start...finish]
        a.is_a?(Bytea) ? Bytea.new(result) : result
      when 'lpad', 'rpad'
        n, fill = [b, 0].max, c || ' '
        return a[0, n] if n <= a.length || fill.empty?
        count = n - a.length
        reserve(context, a.bytesize + fill.bytesize + a.bytesize + (count.div(fill.length) + 1) * fill.bytesize)
        pad = (fill * (count.div(fill.length) + 1))[0, count]
        name == 'lpad' ? pad + a : a + pad
      when 'split_part'
        raise Error.new('22023', 'field position must not be zero') if c.zero?
        parts = b.empty? ? [a] : a.split(b, -1)
        parts[c.positive? ? c - 1 : c] || ''
      when 'concat' then args.compact.map { |v| Types.format(v) }.join
      when 'concat_ws' then a.nil? ? nil : args[1..].compact.map { |v| Types.format(v) }.join(a)
      when 'overlay'
        raise Error.new('22011', 'negative substring length not allowed') if c <= 0
        length = d || b.length
        tail = Types.check_integer(c + length, 'i32')
        (a[0, c - 1] || a) + b + (a[[tail - 1, 0].max..] || '')
      when 'quote_ident' then /\A[a-z_][a-z0-9_$]*\z/.match?(a) && !%w[select from where table user order group limit null true false integer left right xmltable].include?(a) ? a : '"' + a.gsub('"', '""') + '"'
      when 'quote_literal', 'quote_nullable' then a.nil? ? 'NULL' : quote_literal(Types.format(a))
      when 'format' then sql_format(a, args[1..])
      when 'to_hex' then (a.negative? ? a & 0xffffffffffffffff : a).to_s(16)
      when 'encode' then encode(a, b)
      when 'decode' then decode(a, b)
      when 'get_byte'
        raise Error.new('2202E', 'index out of valid range') unless (0...a.bytesize).cover?(b)
        a.getbyte(b)
      when 'set_byte'
        raise Error.new('2202E', 'index out of valid range') unless (0...a.bytesize).cover?(b)
        result = Bytea.new(a); result.setbyte(b, c & 255); result
      when 'get_bit', 'set_bit'
        raise Error.new('2202E', 'index out of valid range') unless (0...(a.bytesize * 8)).cover?(b)
        byte, bit = b.divmod(8)
        return (a.getbyte(byte) >> bit) & 1 if name == 'get_bit'
        raise Error.new('22023', 'new bit must be 0 or 1') unless c == 0 || c == 1
        result = Bytea.new(a); result.setbyte(byte, (a.getbyte(byte) & ~(1 << bit)) | (c << bit)); result
      when 'md5', 'sha224', 'sha256', 'sha384', 'sha512'
        digest = OpenSSL::Digest.new(name.upcase)
        name == 'md5' ? digest.hexdigest(a) : Bytea.new(digest.digest(a))
      when 'like', 'ilike' then like(a, b, escape: c || '\\', insensitive: name == 'ilike', context: context)
      when 'like_escape' then like_escape(a, b)
      when /\Aregexp_/, 'regex_match', 'regex_imatch'
        raise Error.new('0A000', 'bounded regular expressions are not implemented in the Ruby experiment')
      when 'array_append'
        raise Error.new('22000', 'argument must be empty or one-dimensional array') if a && a.dimensions.length > 1
        array((a || []) + [b], a)
      when 'array_prepend'
        raise Error.new('22000', 'argument must be empty or one-dimensional array') if b && b.dimensions.length > 1
        array([a] + (b || []), b)
      when 'array_cat' then array_concat(a, b)
      when 'cardinality' then a.length
      when 'array_ndims' then a.empty? ? nil : a.dimensions.length
      when 'array_length' then b.positive? ? a.dimensions[b - 1] : nil
      when 'array_lower' then b.positive? ? a.lower_bounds[b - 1] : nil
      when 'array_upper' then b.positive? && a.dimensions[b - 1] ? a.lower_bounds[b - 1] + a.dimensions[b - 1] - 1 : nil
      when 'array_dims' then a.empty? ? nil : a.dimensions.each_with_index.map { |dim, i| "[#{a.lower_bounds[i]}:#{a.lower_bounds[i] + dim - 1}]" }.join
      when 'array_position'
        return nil if a.nil?
        start = c.nil? ? lower_bound(a) : c
        index = a.each_index.find { |i| i + lower_bound(a) >= start && Types.compare(a[i], b).zero? }
        index ? index + lower_bound(a) : nil
      when 'array_positions'
        return nil if a.nil?
        ArrayValue.new(a.each_index.filter_map { |i| i + lower_bound(a) if Types.compare(a[i], b).zero? }, element_type: 'i32')
      when 'array_remove' then a.nil? ? nil : array(a.reject { |v| Types.compare(v, b).zero? }, a)
      when 'array_replace'
        return nil if a.nil?
        result = a.map { |v| Types.compare(v, b).zero? ? c : v }
        reserve(context, result.sum { |v| Types.format(v).bytesize })
        array(result, a)
      when 'array_to_string' then a.nil? || b.nil? ? nil : a.filter_map { |v| v.nil? ? c : Types.format(v) }.join(b)
      when 'string_to_array'
        return nil if a.nil?
        values = a.empty? ? [] : b.nil? ? a.chars : b.empty? ? [a] : a.split(b, -1)
        ArrayValue.new(values.map { |v| !c.nil? && v == c ? nil : v }, element_type: 'text')
      when 'unnest' then a.to_a
      when 'generate_series' then series(args, context)
      when 'i32range', 'int4range', 'i64range', 'int8range', 'numrange', 'tsrange', 'tstzrange', 'daterange'
        raise Error.new('22000', 'range bound flags must not be NULL') if args.length == 3 && c.nil?
        raise Error.new('42601', 'invalid range bound flags') if c && !%w[[] [) (] ()].include?(c)
        RangeValue.new(a, b, c || '[)', element_type: Types::RANGES.fetch(Types.normalize(name)))
      when 'isempty' then a.empty
      when 'lower_inc' then a.lower_inc
      when 'upper_inc' then a.upper_inc
      when 'lower_inf' then !a.empty && a.lower.nil?
      when 'upper_inf' then !a.empty && a.upper.nil?
      when 'range_merge' then range_set('merge', a, b)
      when 'now', 'current_timestamp', 'transaction_timestamp', 'statement_timestamp', 'clock_timestamp', 'current_date'
        raise Error.new('0A000', 'clock functions require an injected clock') unless context && context.respond_to?(:clock_value)
        value = context.clock_value(name)
        name == 'current_date' ? Types.cast(value, 'date') : value
      when 'make_date'
        raise Error.new('22008', 'date year cannot be zero') if a.zero?
        DateValue.from_parts(a.negative? ? a + 1 : a, b, c)
      when 'make_timestamp', 'make_timestamptz'
        date = DateValue.from_parts(a, b, c)
        hours, minutes, seconds = args[3], args[4], Decimal.parse(args[5])
        raise Error.new('22008', 'time field value out of range') unless (0..23).cover?(hours) && (0..59).cover?(minutes) && seconds >= 0 && seconds < 60
        TimestampValue.new(date.days * TimestampValue::DAY + (hours * 3600 + minutes * 60) * 1_000_000 + (seconds * 1_000_000).round.coefficient, zoned: name == 'make_timestamptz')
      when 'make_interval'
        values = args + [0] * (7 - args.length)
        IntervalValue.new(values[0] * 12 + values[1], values[2] * 7 + values[3], (values[4] * 3600 + values[5] * 60) * 1_000_000 + (Decimal.parse(values[6]) * 1_000_000).round.coefficient)
      when 'justify_days', 'justify_hours', 'justify_interval' then justify(a, name)
      when 'extract', 'date_part' then extract(a, b, float: name == 'date_part')
      when 'date_trunc' then date_trunc(a, b)
      when 'age'
        raise Error.new('0A000', 'age requires two timestamps in the Ruby experiment') unless b
        age(a, b)
      when 'to_timestamp'
        return DateFormatting.from_text(a, b, context: context) if b
        raise Error.new('22008', 'timestamp cannot be NaN') if a.nan?
        return TimestampValue.new(a, zoned: true) if a.infinite?
        relative = (a - 946_684_800.0) * 1_000_000
        raise Error.new('22008', 'timestamp is out of range') unless relative.finite? && relative >= -(2**63) && relative < 2**63
        floor = relative.floor
        difference = relative - floor
        rounded = difference < 0.5 ? floor : difference > 0.5 ? floor + 1 : floor.even? ? floor : floor + 1
        micros = rounded + 946_684_800_000_000
        raise Error.new('22008', 'timestamp is out of range') unless micros.between?(-(2**63) + 1, 2**63 - 2)
        TimestampValue.new(micros, zoned: true)
      when 'to_number' then NumberFormatting.to_number(a, b, context: context)
      when 'to_date' then DateFormatting.from_text(a, b, date: true, context: context)
      when 'to_char'
        raise Error.new('0A000', 'format template functions are not implemented in the Ruby experiment')
      when 'uuid_extract_version' then (a[19].to_i(16) & 12) == 8 ? a[14].to_i(16) : nil
      when 'uuid_extract_timestamp'
        return nil unless (a[19].to_i(16) & 12) == 8
        version = a[14].to_i(16)
        hex = a.delete('-')
        if version == 7
          TimestampValue.new(hex[0, 12].to_i(16) * 1000, zoned: true)
        elsif version == 1
          ticks = ((hex[12, 4].to_i(16) & 0xfff) << 48) | (hex[8, 4].to_i(16) << 32) | hex[0, 8].to_i(16)
          TimestampValue.new((ticks - 122_192_928_000_000_000).div(10), zoned: true)
        end
      when 'uuidv4', 'uuidv7', 'gen_random_uuid'
        raise Error.new('0A000', 'UUID generators require an injected entropy source') unless context && context.respond_to?(:uuid_value)
        context.uuid_value(name, args[0])
      when 'nextval', 'currval', 'lastval', 'setval'
        raise Error.new('0A000', 'sequence state is unavailable') unless context && context.respond_to?(:sequence_value)
        context.sequence_value(name, args)
      when 'current_setting'
        raise Error.new('0A000', 'settings are unavailable') unless context && context.respond_to?(:setting_value)
        context.setting_value(a, b)
      when 'to_json', 'to_jsonb', 'array_to_json', 'row_to_json', 'json_scalar'
        return nil if a.nil?
        JsonValue.from_ruby(a, binary: name == 'to_jsonb')
      when 'json_build_array', 'jsonb_build_array' then JsonValue.from_ruby(args, binary: name.start_with?('jsonb'))
      when 'json_build_object', 'jsonb_build_object'
        raise Error.new('22023', 'argument list must have even number of elements') unless args.length.even?
        object = {}
        args.each_slice(2) do |key, value|
          raise Error.new('22023', 'JSON object key must not be null') if key.nil?
          object[Types.format(key)] = JsonValue.from_ruby(value).value
        end
        JsonValue.new(object, binary: name.start_with?('jsonb'))
      when 'json_typeof', 'jsonb_typeof' then a.type_name
      when 'json_array_length', 'jsonb_array_length'
        raise Error.new('22023', 'cannot get array length of a non-array') unless a.value.is_a?(Array)
        a.value.length
      when 'json_object_keys', 'jsonb_object_keys'
        raise Error.new('22023', 'cannot call object_keys on a non-object') unless a.value.is_a?(Hash)
        a.value.keys.sort_by { |k| [k.bytesize, k.b] }
      when 'json_array_elements', 'jsonb_array_elements', 'json_array_elements_text', 'jsonb_array_elements_text'
        raise Error.new('22023', 'cannot extract elements from a scalar') unless a.value.is_a?(Array)
        a.value.map { |v| wrapped = JsonValue.new(v, binary: a.binary); name.end_with?('_text') ? wrapped.text : wrapped }
      when 'json_each', 'jsonb_each', 'json_each_text', 'jsonb_each_text'
        raise Error.new('22023', 'cannot call each on a non-object') unless a.value.is_a?(Hash)
        a.value.keys.sort_by { |k| [k.bytesize, k.b] }.map { |k| v = JsonValue.new(a.value[k], binary: a.binary); [k, name.end_with?('_text') ? v.text : v] }
      when 'jsonb_pretty'
        rendered = a.pretty
        reserve(context, a.to_s.bytesize + rendered.bytesize)
        rendered
      when 'json_strip_nulls', 'jsonb_strip_nulls' then JsonValue.new(strip_nulls(a.decoded_value, b || false), binary: name.start_with?('jsonb'))
      when 'json_serialize' then a.to_s
      when /\Ajsonb?_path_/, 'json_exists', 'json_value', 'json_query'
        raise Error.new('0A000', 'SQL/JSON path is not implemented in the Ruby experiment')
      else raise Error.new('42883', "function #{name} does not exist in the experimental Ruby core")
      end
    rescue Math::DomainError, FloatDomainError
      raise Error.new('22003', 'argument outside function domain')
    end

    def reserve(context, bytes)
      raise Error.new('54000', 'requested result is too large') if bytes > 1_073_741_823
      raise Error.new('54P04', 'scalar allocation limit exceeded') if bytes > 64 * 1024 * 1024 && !context
      if context
        context.charge(bytes) if context.respond_to?(:charge)
        context.consume_scalar_bytes(bytes) if context.respond_to?(:consume_scalar_bytes)
      end
    end
    def array(values, source)
      ArrayValue.new(values, element_type: source.respond_to?(:element_type) ? source.element_type : nil, lower_bound: source.respond_to?(:lower_bound) ? source.lower_bound : 1, dimensions: source.is_a?(ArrayValue) && values.length == source.length ? source.dimensions : nil, lower_bounds: source.is_a?(ArrayValue) && values.length == source.length ? source.lower_bounds : nil)
    end
    def array_concat(a, b)
      return b if a.nil?
      return a if b.nil?
      return b if a.empty?
      return a if b.empty?
      ad, bd = a.dimensions, b.dimensions
      if ad.length == bd.length
        raise Error.new('2202E', 'cannot concatenate incompatible arrays') unless ad[1..] == bd[1..]
        dims = [ad[0] + bd[0]] + ad[1..]
        bounds = a.lower_bounds
      elsif ad.length == bd.length + 1
        raise Error.new('2202E', 'cannot concatenate incompatible arrays') unless ad[1..] == bd
        dims = [ad[0] + 1] + ad[1..]
        bounds = a.lower_bounds
      elsif bd.length == ad.length + 1
        raise Error.new('2202E', 'cannot concatenate incompatible arrays') unless bd[1..] == ad
        dims = [bd[0] + 1] + bd[1..]
        bounds = b.lower_bounds
      else raise Error.new('2202E', 'cannot concatenate incompatible arrays')
      end
      ArrayValue.new(a + b, element_type: a.element_type, dimensions: dims, lower_bounds: bounds)
    end
    def lower_bound(value) = value.respond_to?(:lower_bound) ? value.lower_bound : 1
    def like_escape(pattern, escape)
      raise Error.new('22025', 'invalid escape string') unless escape.empty? || escape.length == 1
      return pattern if escape == '\\'
      result, escaped = +'', false
      pattern.each_char do |char|
        if escaped
          result << char
          escaped = false
        elsif !escape.empty? && char == escape
          result << '\\'
          escaped = true
        elsif char == '\\'
          result << '\\\\'
        else
          result << char
        end
      end
      result
    end
    def like(text, pattern, escape: '\\', insensitive: false, context: nil)
      raise Error.new('22025', 'invalid escape string') unless escape.empty? || escape.length == 1
      text = text.tr('A-Z', 'a-z') if insensitive
      pattern = pattern.tr('A-Z', 'a-z') if insensitive
      tokens, escaped = [], false
      pattern.each_char do |char|
        if escaped then tokens << [:literal, char]; escaped = false
        elsif !escape.empty? && char == escape then escaped = true
        elsif char == '%' then tokens << [:many, nil] unless tokens.last&.first == :many
        elsif char == '_' then tokens << [:one, nil]
        else tokens << [:literal, char]
        end
      end
      tokens << [:escape, nil] if escaped
      chars = text.chars
      input = token = 0
      wildcard = retry_input = nil
      while input < chars.length
        kind, literal = tokens[token]
        raise Error.new('22025', 'LIKE pattern must not end with escape character') if kind == :escape
        if kind == :one || kind == :literal && literal == chars[input]
          input += 1
          token += 1
        elsif kind == :many
          wildcard, retry_input = token, input
          token += 1
        elsif wildcard
          retry_input += 1
          input = retry_input
          token = wildcard + 1
        else
          return false
        end
      end
      token += 1 while tokens[token]&.first == :many
      token == tokens.length
    end
    def regex(_pattern, _flags = '') = raise(Error.new('0A000', 'bounded regular expressions are not implemented in the Ruby experiment'))
    def math(name, args, context: nil)
      a, b = args
      return DecimalMath.call(name, args, context: context) if a.is_a?(Decimal) || b.is_a?(Decimal)
      raise Error.new('42883', "function #{name} does not accept integer arguments") if args.all? { |v| v.is_a?(Integer) }
      result = case name
      when 'sqrt' then Math.sqrt(a.to_f)
      when 'exp' then Math.exp(a.to_f)
      when 'ln' then Math.log(a.to_f)
      when 'log10' then Math.log10(a.to_f)
      when 'log' then b ? Math.log(b.to_f, a.to_f) : Math.log10(a.to_f)
      else a.to_f**b.to_f
      end
      raise Error.new('22003', 'float out of range') if result.is_a?(Complex) || (result.infinite? && args.all? { |v| v.to_f.finite? })
      result
    end
    def quote_literal(text)
      text.include?('\\') ? "E'" + text.gsub('\\') { '\\\\' }.gsub("'", "''") + "'" : "'" + text.gsub("'", "''") + "'"
    end
    def sql_format(template, values)
      raise Error.new('22004', 'null format string') if template.nil?
      next_index = 0
      template.gsub(/%(?:(\d+)\$)?(-)?(\d+)?([sIL%])/) do
        match = Regexp.last_match
        next '%' if match[4] == '%'
        index = match[1] ? match[1].to_i - 1 : next_index
        raise Error.new('22023', 'too few arguments for format') unless index < values.length
        next_index = index + 1
        value = values[index]
        rendered = case match[4]
                   when 's' then value.nil? ? '' : Types.format(value)
                   when 'L' then value.nil? ? 'NULL' : quote_literal(Types.format(value))
                   when 'I'
                     raise Error.new('22004', 'null values cannot be formatted as SQL identifiers') if value.nil?
                     call('quote_ident', [Types.format(value)])
                   end
        width = match[3].to_i
        match[2] ? rendered.ljust(width) : rendered.rjust(width)
      end
    end
    def encode(bytes, format)
      case format.downcase
      when 'hex' then bytes.unpack1('H*')
      when 'base64' then [bytes].pack('m').strip
      when 'escape'
        output = +''
        bytes.each_byte { |byte| output << ESCAPE_BYTES[byte] }
        output
      else raise Error.new('22023', 'unrecognized encoding')
      end
    end
    def decode(text, format)
      case format.downcase
when 'hex'
  raise Error.new('22023', 'invalid hexadecimal input') unless /\A(?:[ \t\r\n]*[0-9a-fA-F]{2})*[ \t\r\n]*\z/.match?(text)
  Bytea.new([text.delete(" \t\r\n")].pack('H*'))
      when 'base64'
        compact = text.delete(" \t\r\n")
        raise Error.new('22023', 'invalid base64 input') unless /\A(?:[A-Za-z0-9+\/]{4})*(?:[A-Za-z0-9+\/]{2}==|[A-Za-z0-9+\/]{3}=)?\z/.match?(compact)
        Bytea.new(compact.unpack1('m0'))
      when 'escape' then Types.cast(text, 'bytea')
      else raise Error.new('22023', 'unrecognized encoding')
      end
    end
    def range_set(op, a, b)
      return b if a.empty && op != '*'
      return a if b.empty && op != '*'
      return RangeValue.new(nil, nil, element_type: a.element_type, empty: true) if a.empty || b.empty
      left, right = a <= b ? [a, b] : [b, a]
      if op == '-'
        return a unless a.overlaps?(b)
        return RangeValue.new(nil, nil, element_type: a.element_type, empty: true) if b.contains?(a)
        raise Error.new('22000', 'result of range difference would not be contiguous') if a.contains?(b) && a.lower != b.lower && a.upper != b.upper
        if left.equal?(a)
          return RangeValue.new(a.lower, b.lower, (a.lower_inc ? '[' : '(') + (b.lower_inc ? ')' : ']'), element_type: a.element_type)
        end
        return RangeValue.new(b.upper, a.upper, (b.upper_inc ? '(' : '[') + (a.upper_inc ? ']' : ')'), element_type: a.element_type)
      end
      if op == '*'
        return RangeValue.new(nil, nil, element_type: a.element_type, empty: true) unless a.overlaps?(b)
        lower, lower_inc = right.lower, right.lower_inc
        upper_source = if a.upper.nil? then b elsif b.upper.nil? then a elsif Types.compare(a.upper, b.upper) < 0 then a else b end
        return RangeValue.new(lower, upper_source.upper, (lower_inc ? '[' : '(') + (upper_source.upper_inc ? ']' : ')'), element_type: a.element_type)
      end
      raise Error.new('22000', 'result of range union would not be contiguous') if op == '+' && !a.overlaps?(b) && !a.adjacent?(b)
      upper_source = if a.upper.nil? then a elsif b.upper.nil? then b elsif Types.compare(a.upper, b.upper) > 0 then a else b end
      RangeValue.new(left.lower, upper_source.upper, (left.lower_inc ? '[' : '(') + (upper_source.upper_inc ? ']' : ')'), element_type: a.element_type)
    end
    def series(args, context)
      first, last, step = args
      step ||= 1
      raise Error.new('22023', 'step size cannot equal zero') if step.respond_to?(:zero?) && step.zero?
      ascending = step.is_a?(IntervalValue) ? step.span.positive? : step.positive?
      result, value = [], first
      while ascending ? Types.compare(value, last) <= 0 : Types.compare(value, last) >= 0
        context&.charge(1)
        result << value
        raise Error.new('54000', 'series allocation requires a bounded streaming consumer') if result.length > 1_000_000
        value = value.is_a?(TimestampValue) ? Types.datetime_binary('+', value, step) : value + step
      end
      result
    end
    def justify(interval, name)
      months, days, micros = interval.months, interval.days, interval.micros
      if name != 'justify_days'
        whole = micros.negative? ? -(-micros / TimestampValue::DAY) : micros / TimestampValue::DAY
        days += whole
        micros -= whole * TimestampValue::DAY
      end
      if name != 'justify_hours'
        whole = days.negative? ? -(-days / 30) : days / 30
        months += whole
        days -= whole * 30
        sign = days.zero? && name == 'justify_interval' ? micros <=> 0 : days <=> 0
        if months.positive? && sign.negative?
          months -= 1
          days += 30
        elsif months.negative? && sign.positive?
          months += 1
          days -= 30
        end
      end
      if name != 'justify_days'
        if days.positive? && micros.negative?
          days -= 1
          micros += TimestampValue::DAY
        elsif days.negative? && micros.positive?
          days += 1
          micros -= TimestampValue::DAY
        end
      end
      IntervalValue.new(months, days, micros)
    end
    def extract(unit, value, float: false)
      unit = unit.downcase
      if value.is_a?(IntervalValue)
        result = case unit
                 when 'year', 'years' then value.months / 12
                 when 'month', 'months' then value.months.remainder(12)
                 when 'day', 'days' then value.days
                 when 'hour', 'hours' then value.micros / 3_600_000_000
                 when 'minute', 'minutes' then (value.micros / 60_000_000).remainder(60)
                 when 'second', 'seconds' then Decimal.new(value.micros.remainder(60_000_000), 6)
                 when 'epoch' then Decimal.new(value.span, 6)
                 else raise Error.new('22023', "unsupported interval unit #{unit}")
                 end
      else
        micros = value.is_a?(DateValue) ? value.days * TimestampValue::DAY : value.micros
        days, time = micros.divmod(TimestampValue::DAY)
        year, month, day = DateValue.new(days).parts
        result = case unit
                 when 'year', 'years' then year
                 when 'month', 'months' then month
                 when 'day', 'days' then day
                 when 'hour', 'hours' then time / 3_600_000_000
                 when 'minute', 'minutes' then (time / 60_000_000) % 60
                 when 'second', 'seconds' then Decimal.new(time % 60_000_000, 6)
                 when 'millisecond', 'milliseconds' then Decimal.new(time % 60_000_000, 3)
                 when 'microsecond', 'microseconds' then time % 60_000_000
                 when 'epoch' then Decimal.new(micros, 6)
                 when 'dow' then (days + 4) % 7
                 when 'isodow' then (days + 3) % 7 + 1
                 when 'doy' then days - DateValue.from_parts(year, 1, 1).days + 1
                 when 'quarter' then (month - 1) / 3 + 1
                 when 'decade' then year / 10
                 when 'century' then (year + 99) / 100
                 when 'millennium' then (year + 999) / 1000
                 when 'timezone', 'timezone_hour', 'timezone_minute' then 0
                 else raise Error.new('22023', "unsupported timestamp unit #{unit}")
                 end
      end
      float ? result.to_f : Decimal.parse(result)
    end
    def date_trunc(unit, value)
      micros = value.is_a?(DateValue) ? value.days * TimestampValue::DAY : value.micros
      day, time = micros.divmod(TimestampValue::DAY)
      year, month, date = DateValue.new(day).parts
      case unit.downcase
      when 'year' then month = date = 1; time = 0
      when 'quarter' then month = (month - 1) / 3 * 3 + 1; date = 1; time = 0
      when 'month' then date = 1; time = 0
      when 'day' then time = 0
      when 'hour' then time = time.div(3_600_000_000) * 3_600_000_000
      when 'minute' then time = time.div(60_000_000) * 60_000_000
      when 'second' then time = time.div(1_000_000) * 1_000_000
      when 'milliseconds' then time = time.div(1000) * 1000
      when 'microseconds' then nil
      else raise Error.new('22023', 'unsupported date_trunc unit')
      end
      TimestampValue.new(DateValue.from_parts(year, month, date).days * TimestampValue::DAY + time, zoned: value.is_a?(TimestampValue) && value.zoned)
    end
    def age(a, b, ordered: false)
      a = TimestampValue.parse(a)
      b = TimestampValue.parse(b)
      return -age(b, a) if !ordered && a < b
      ay, am, ad = DateValue.new(a.micros.div(TimestampValue::DAY)).parts
      by, bm, bd = DateValue.new(b.micros.div(TimestampValue::DAY)).parts
      months, days, micros = (ay - by) * 12 + am - bm, ad - bd, a.micros % TimestampValue::DAY - b.micros % TimestampValue::DAY
      if micros.negative? then days -= 1; micros += TimestampValue::DAY end
      if days.negative?
        months -= 1
        next_month = bm == 12 ? DateValue.from_parts(by + 1, 1, 1) : DateValue.from_parts(by, bm + 1, 1)
        days += next_month.days - DateValue.from_parts(by, bm, 1).days
      end
      IntervalValue.new(months, days, micros)
    end
    def strip_nulls(value, arrays)
      case value
      when Hash then value.reject { |_k, v| v.nil? }.transform_values { |v| strip_nulls(v, arrays) }
      when Array then (arrays ? value.compact : value).map { |v| strip_nulls(v, arrays) }
      else value
      end
    end
  end
end
