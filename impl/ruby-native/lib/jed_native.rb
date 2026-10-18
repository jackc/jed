# frozen_string_literal: true

require_relative 'jed_native/error'
require_relative 'jed_native/types'
require_relative 'jed_native/functions'
require_relative 'jed_native/jsonpath'
require_relative 'jed_native/parser'
require_relative 'jed_native/result'
require_relative 'jed_native/engine'
require_relative 'jed_native/expressions'
require_relative 'jed_native/windows'
require_relative 'jed_native/aggregates'
require_relative 'jed_native/schema'
require_relative 'jed_native/constraints'
require_relative 'jed_native/prepared_select'
require_relative 'jed_native/query_composition'
require_relative 'jed_native/bindings'
require_relative 'jed_native/regex_functions'
require_relative 'jed_native/jsonpath_functions'
require_relative 'jed_native/json_sql'
require_relative 'jed_native/json_operations'
require_relative 'jed_native/json_text'
require_relative 'jed_native/json_relations'
require_relative 'jed_native/json_images'
require_relative 'jed_native/formatting_number'
require_relative 'jed_native/formatting_datetime'
require_relative 'jed_native/formatting_parse'
require_relative 'jed_native/mutations'
require_relative 'jed_native/sequences'
require_relative 'jed_native/environment'
require_relative 'jed_native/views'
require_relative 'jed_native/ctes'
require_relative 'jed_native/subqueries'
require_relative 'jed_native/introspection'
require_relative 'jed_native/planner'
require_relative 'jed_native/indexes'
require_relative 'jed_native/resolution'
require_relative 'jed_native/logical_memory'
require_relative 'jed_native/scalar_kernels'
require_relative 'jed_native/scans'
require_relative 'jed_native/interval_scans'
require_relative 'jed_native/timezones'
require_relative 'jed_native/range_constructors'
require_relative 'jed_native/domains'

module JedNative
  VERSION = '0.0.0.experimental'

  class Statement
    attr_reader :ast
    def initialize(sql)
      statements = Parser.new(sql).parse
      raise Error.new('42601', 'expected one SQL statement') unless statements.length == 1
      @ast = statements.first
    end
  end

  class Database
    attr_reader :engine
    attr_accessor :max_sql_length

    def initialize(max_sql_length: 1_048_576, **options)
      @max_sql_length = max_sql_length
      @engine = Engine.new(**options)
    end

    def query(sql, *params) = query_prepared(prepare(sql), *params)
    def execute(sql, *params) = query(sql, *params)
    def prepare(sql)
      raise Error.new('54000', 'SQL input exceeds max_sql_length') if @max_sql_length.positive? && sql.bytesize > @max_sql_length
      Statement.new(sql)
    end
    def query_prepared(statement, *params)
      params.map! { |value| HostValues.copy(value) }
      @engine.run(statement.ast, params).detach
    end
    alias execute_prepared query_prepared

    def execute_script(sql)
      statements = Parser.new(sql).parse
      if statements.any? { |s| %i[begin commit rollback savepoint release rollback_to].include?(s[:kind]) }
        raise Error.new('0A000', 'transaction control in execute_script')
      end
      raise Error.new('25001', 'execute_script inside a transaction is not implemented') if @engine.transaction_open?
      execute('BEGIN')
      begin
        results = statements.map { |statement| @engine.run(statement).detach }
        execute('COMMIT')
        results
      rescue StandardError
        execute('ROLLBACK')
        raise
      end
    end

    def transaction
      execute('BEGIN')
      value = yield self
      execute('COMMIT')
      value
    rescue Exception
      execute('ROLLBACK')
      raise
    end

    def close = @engine.close
    def render(value, type = nil) = @engine.render_value(value, type)
    def timezone = @engine.timezone
    def timezone=(name)
      @engine.timezone = name
    end
    def attach(name, source = :memory, mode = :read_write, **options) = @engine.attach(name, source, mode, **options)
    def detach(name) = @engine.detach(name)
    def max_cost=(value)
      @engine.max_cost = value
    end
  end

  def self.memory(**options)
    db = Database.new(**options)
    return db unless block_given?
    begin
      yield db
    ensure
      db.close
    end
  end

  def self.create(path, **options, &block) = file_database(path, true, options, &block)
  def self.open(path, **options, &block) = file_database(path, false, options, &block)

  def self.file_database(path, create, options)
    require_relative 'jed_native/persistence'
    engine_options = options.select { |key, _| %i[environment max_cost max_scalar_bytes max_sql_length lifetime_max_cost].include?(key) }
    storage_options = options.reject { |key, _| engine_options.key?(key) }
    db = Database.new(**engine_options)
    db.engine.attach_storage(path: path, create: create, **storage_options)
    return db unless block_given?
    begin
      yield db
    ensure
      db.close
    end
  end
end
