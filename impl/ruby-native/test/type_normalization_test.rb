# frozen_string_literal: true

require_relative '../lib/jed_native'

module NativeTypeNormalizationTests
  module_function

  def assert(value, message) = (raise(message) unless value)

  def run
    types = JedNative::Types
    {
      'i32' => 'i32', 'unknown' => 'unknown', 'null' => 'unknown',
      '  integer  ' => 'i32', "double\t precision" => 'f64',
      'timestamp with time zone' => 'timestamptz',
      'numeric(12, 3)' => 'decimal(12,3)', 'varchar( 10 )' => 'text(10)',
      'integer[3][4]' => 'i32[]', 'i64[]' => 'i64[]',
      'My_Type' => 'My_Type', 'INTEGER' => 'INTEGER', :integer => 'i32'
    }.each do |input, expected|
      assert(types.normalize(input) == expected, "normalization changed for #{input.inspect}")
      assert(types.base(input) == expected.sub(/\(.*\)\z/, ''), "base type changed for #{input.inspect}")
    end
    external = +'i32'
    normalized = types.normalize(external)
    external.replace('text')
    assert(normalized == 'i32', 'canonical type retained a mutable caller string')
    types::INTEGER_BITS.each do |type, bits|
      lower, upper = -(1 << (bits - 1)), (1 << (bits - 1)) - 1
      [lower, -1, 0, 1, upper].each { |value| assert(types.check_integer(value, type) == value, "valid #{type} boundary rejected") }
      [lower - 1, upper + 1].each do |value|
        begin
          types.check_integer(value, type)
          raise "invalid #{type} boundary accepted"
        rescue JedNative::Error => error
          assert(error.sqlstate == '22003', 'integer overflow SQLSTATE changed')
        end
      end
    end
    assert(types.check_integer(32_767, 'smallint') == 32_767, 'integer alias admission changed')
    puts 'ruby-native type normalization: canonical names, caller ownership, external syntax and exact signed bounds passed'
  end
end

NativeTypeNormalizationTests.run
