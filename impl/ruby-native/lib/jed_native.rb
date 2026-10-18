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
require_relative 'jed_native/scalar_work'
require_relative 'jed_native/scans'
require_relative 'jed_native/interval_scans'
require_relative 'jed_native/auxiliary_scans'
require_relative 'jed_native/hash_joins'
require_relative 'jed_native/join_planner'
require_relative 'jed_native/join_pushdown'
require_relative 'jed_native/index_nested_loops'
require_relative 'jed_native/streaming_select'
require_relative 'jed_native/timezones'
require_relative 'jed_native/range_constructors'
require_relative 'jed_native/datetime_functions'
require_relative 'jed_native/argument_resolution'
require_relative 'jed_native/collations'
require_relative 'jed_native/domains'
require_relative 'jed_native/queryplan'
require_relative 'jed_native/join_explain'
require_relative 'jed_native/access_planning'
require_relative 'jed_native/statistics_estimates'
require_relative 'jed_native/create_table_as'
require_relative 'jed_native/query_columns'
require_relative 'jed_native/collation_metadata'
require_relative 'jed_native/width_bucket'
require_relative 'jed_native/temporal_modifiers'
require_relative 'jed_native/datetime_arithmetic'
require_relative 'jed_native/date_clock'
require_relative 'jed_native/composite_values'
require_relative 'jed_native/strict_context'
require_relative 'jed_native/array_semantics'
require_relative 'jed_native/function_resolution'
require_relative 'jed_native/capabilities'
require_relative 'jed_native/filter_predicates'
require_relative 'jed_native/sorted_select'
require_relative 'jed_native/query_memory'

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
      raise Error.new('54000', 'SQL input exceeds max_sql_length') if @max_sql_length.positive? && sql.bytesize > @max_sql_length
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
    def grant(privileges, object) = @engine.grant(privileges, object)
    def revoke(privileges, object) = @engine.revoke(privileges, object)
    def reset_privileges = @engine.reset_privileges
    def default_privileges=(value)
      @engine.default_privileges = value
    end
    def allow_ddl=(value)
      @engine.allow_ddl = value
    end
    def allow_temp_ddl=(value)
      @engine.allow_temp_ddl = value
    end
    def max_cost=(value)
      @engine.max_cost = value
    end
    def max_query_memory_bytes=(value)
      @engine.max_query_memory_bytes = value
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
    engine_options = options.select { |key, _| %i[environment max_cost max_scalar_bytes max_sql_length lifetime_max_cost allow_ddl allow_temp_ddl default_privileges max_query_memory_bytes].include?(key) }
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
