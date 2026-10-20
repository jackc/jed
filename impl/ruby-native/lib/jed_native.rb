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
require_relative 'jed_native/nway_joins'
require_relative 'jed_native/record_shapes'
require_relative 'jed_native/join_pushdown'
require_relative 'jed_native/body_pushdown'
require_relative 'jed_native/index_nested_loops'
require_relative 'jed_native/streaming_select'
require_relative 'jed_native/timezones'
require_relative 'jed_native/range_constructors'
require_relative 'jed_native/datetime_functions'
require_relative 'jed_native/argument_resolution'
require_relative 'jed_native/collations'
require_relative 'jed_native/collation_upgrade'
require_relative 'jed_native/domains'
require_relative 'jed_native/queryplan'
require_relative 'jed_native/join_explain'
require_relative 'jed_native/nway_explain'
require_relative 'jed_native/relation_explain'
require_relative 'jed_native/access_planning'
require_relative 'jed_native/tuple_intervals'
require_relative 'jed_native/ordered_indexes'
require_relative 'jed_native/subquery_access'
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
require_relative 'jed_native/scalar_edges'
require_relative 'jed_native/capabilities'
require_relative 'jed_native/filter_predicates'
require_relative 'jed_native/sorted_select'
require_relative 'jed_native/spill_storage'
require_relative 'jed_native/spill_select'
require_relative 'jed_native/spill_aggregates'
require_relative 'jed_native/explain_composition'
require_relative 'jed_native/explain_subqueries'
require_relative 'jed_native/query_memory'
require_relative 'jed_native/contradictions'
require_relative 'jed_native/opaque_definitions'
require_relative 'jed_native/predicate_implication'
require_relative 'jed_native/schema_resolution'
require_relative 'jed_native/mutation_resolution'
require_relative 'jed_native/foreign_key_work'
require_relative 'jed_native/function_relations'
require_relative 'jed_native/view_mutations'
require_relative 'jed_native/storage_limits'
require_relative 'jed_native/deep_queries'
require_relative 'jed_native/paged_access'
require_relative 'jed_native/paged_deltas'
require_relative 'jed_native/gin_queries'
require_relative 'jed_native/join_edges'

module JedNative
  Engine.prepend(RecordShapes)
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
    attr_reader :max_sql_length

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
    def upgrade_collations = @engine.upgrade_collations
    def set_max_storage_bytes(bytes, database = 'main') = @engine.set_max_storage_bytes(bytes, database)
    def max_storage_bytes = @engine.instance_variable_get(:@storage_limits).fetch('main', 0)
    def max_storage_bytes=(bytes)
      set_max_storage_bytes(bytes)
    end
    def max_temp_storage_bytes = @engine.max_temp_storage_bytes
    def max_temp_storage_bytes=(bytes)
      configure_session_setting(:max_temp_storage_bytes, bytes)
    end
    def max_sql_length=(bytes)
      configure_session_setting(:max_sql_length, bytes)
    end
    def render(value, type = nil) = @engine.render_value(value, type)
    def timezone = @engine.timezone
    def timezone=(name)
      configure_session_setting(:timezone, name)
    end
    def attach(name, source = :memory, mode = :read_write, **options) = @engine.attach(name, source, mode, **options)
    def detach(name) = @engine.detach(name)
    def grant(privileges, object) = configure_privileges(:grant, privileges, object)
    def revoke(privileges, object) = configure_privileges(:revoke, privileges, object)
    def reset_privileges = configure_privileges(:reset_privileges)
    def default_privileges = @engine.default_privileges.dup
    def default_privileges=(value)
      configure_session_setting(:default_privileges, value)
    end
    def allow_ddl = @engine.allow_ddl
    def allow_ddl=(value)
      configure_session_setting(:allow_ddl, value)
    end
    def allow_temp_ddl = @engine.allow_temp_ddl
    def allow_temp_ddl=(value)
      configure_session_setting(:allow_temp_ddl, value)
    end
    def max_cost = @engine.max_cost
    def max_cost=(value)
      configure_session_setting(:max_cost, value)
    end
    def lifetime_max_cost = @engine.lifetime_max_cost
    def lifetime_max_cost=(value)
      configure_session_setting(:lifetime_max_cost, value)
    end
    def max_scalar_bytes = @engine.max_scalar_bytes
    def max_scalar_bytes=(value)
      configure_session_setting(:max_scalar_bytes, value)
    end
    def max_query_memory_bytes = @engine.max_query_memory_bytes
    def max_query_memory_bytes=(value)
      configure_session_setting(:max_query_memory_bytes, value)
    end
    def work_mem = @engine.work_mem
    def work_mem=(value)
      configure_session_setting(:work_mem, value)
    end
    def set_work_mem(value) = configure_session_setting(:work_mem, value)

    def path = @engine.instance_variable_get(:@storage)&.path&.dup
    def page_size = @engine.instance_variable_get(:@storage)&.page_size || @engine.instance_variable_get(:@native_page_size)
    def read_only? = !!@engine.instance_variable_get(:@storage)&.read_only?
    def resident_leaves
      if (storage = @engine.instance_variable_get(:@storage))
        storage.instance_variable_get(:@image).decoded_leaf_cache.size
      elsif (domain = @engine.instance_variable_get(:@memory_domains)['main'])
        pages = domain.instance_variable_get(:@pages)
        domain.instance_variable_get(:@live_pages).count { |page| pages[page]&.getbyte(0) == FileFormat::PAGE_LEAF }
      else
        0
      end
    end

    private

    def image_from_engine(engine, page_size, txid)
      codec = engine.send(:statistics_codec)
      raise Error.new('0A000', 'invalid page size') unless codec.valid_page_size?(page_size)
      unless txid.is_a?(Integer) && txid.between?(0, 0xFFFF_FFFF_FFFF_FFFF)
        raise Error.new('54000', 'transaction id overflow')
      end
      pages = PageBuffer.new(page_size, 3)
      meta = PhysicalImage.new(codec, page_size).write(engine.export_storage, txid) do |page, bytes|
        pages.resize(page + 1) if page >= pages.page_count
        pages[page * page_size, page_size] = bytes
      end
      pages.resize(meta[:page_count])
      pages.to_s
    end

    def apply_session_setting(name, value)
      return @max_sql_length = value if name == :max_sql_length
      @engine.public_send("#{name}=", value)
      @engine.public_send(name)
    end

    def configure_session_setting(name, value) = apply_session_setting(name, value)
    def configure_privileges(operation, *arguments) = @engine.public_send(operation, *arguments)
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

  def self.from_image(bytes, **options)
    require_relative 'jed_native/persistence'
    db = Database.new(**options)
    codec = FileFormat.new(collation_key: ->(name, value) { db.engine.resolve_collation(name).sort_key(value) })
    state = codec.decode_image(bytes, lazy: true)
    db.engine.instance_variable_set(:@native_page_size, bytes.byteslice(8, 4).unpack1('N'))
    db.engine.send(:import_storage, state)
    db.engine.seed_memory_image(bytes, state)
    return db unless block_given?
    begin
      yield db
    ensure
      db.close
    end
  rescue Error
    db&.close
    raise
  rescue StandardError => error
    db&.close
    raise Error.new('XX001', error.message)
  end

  def self.file_database(path, create, options)
    require_relative 'jed_native/persistence'
    engine_options = options.select { |key, _| %i[environment max_cost max_scalar_bytes max_sql_length lifetime_max_cost allow_ddl allow_temp_ddl default_privileges max_query_memory_bytes work_mem max_temp_storage_bytes].include?(key) }
    storage_options = options.reject { |key, _| engine_options.key?(key) }
    db = Database.new(**engine_options)
    db.engine.attach_storage(path: path, create: create, **storage_options)
    db.engine.set_max_storage_bytes(storage_options[:max_storage_bytes]) if storage_options.key?(:max_storage_bytes)
    return db unless block_given?
    begin
      yield db
    ensure
      db.close
    end
  end
end

require_relative 'jed_native/sessions'
# Cursor depth must enclose spill preflight too: shape planning can evaluate
# independent subqueries, whose rows belong to the enclosing query.
require_relative 'jed_native/spill_semantics'
require_relative 'jed_native/cursors'
require_relative 'jed_native/page_service_database'
require_relative 'jed_native/deep_expressions'
require_relative 'jed_native/host_functions'

require_relative 'jed_native/deep_stack'

require_relative 'jed_native/paged_constraints'
require_relative 'jed_native/storage_rewrite'

require_relative 'jed_native/host_batches'
require_relative 'jed_native/storage_operations'
require_relative 'jed_native/fast_evaluation'
require_relative 'jed_native/statement_cache'
require_relative 'jed_native/compiled_expression'
require_relative 'jed_native/compiled_pipeline'
require_relative 'jed_native/compiled_ordering'
require_relative 'jed_native/compiled_aggregate'
require_relative 'jed_native/compiled_relation'
require_relative 'jed_native/compiled_plans'
require_relative 'jed_native/prepared_joins'
require_relative 'jed_native/query_walks'
require_relative 'jed_native/expression_walks'
