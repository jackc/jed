# frozen_string_literal: true

require_relative '../lib/jed_native'
require 'tmpdir'
module NativeHostFunctionTests
  module_function
  def assert(value, message) = (raise(message) unless value)
  def error(code)
    yield
    raise "expected #{code}"
  rescue JedNative::Error => caught
    assert(caught.sqlstate == code, "expected #{code}, got #{caught.sqlstate}: #{caught.message}")
  end
  def environment(version = 1)
    JedNative::Environment.new.register_function(name: 'twice', args: ['i64'], returns: 'i64',
      volatility: :immutable, cost: 3, component_id: 'test/twice', semantic_version: version) { |value| value * 2 }
  end
  def run
    calls = []
    base = environment
    env = base.register_function(name: 'greet', args: ['text'], returns: 'text') { |text| calls << text; text.upcase }
    env = env.register_function(name: 'twice', args: ['text'], returns: 'text') { |text| text + text }
    env = env.register_function(name: 'abs', args: ['i64'], returns: 'i64') { |_value| 99 }
    error('42723') { env.register_function(name: 'twice', args: ['i64'], returns: 'text') { 'x' } }
    error('42704') { env.register_function(name: 'bad', args: ['i64[]'], returns: 'i64') { 1 } }
    error('22023') { env.register_function(name: 'bad', args: [], returns: 'i64', cost: -1) { 1 } }
    JedNative.memory(environment: env) do |core|
      s = core.session
      assert(s.execute("select twice(3),twice('a'),abs(-4)").rows == [[6,'aa',4]], 'signature overload and built-in priority')
      assert(s.execute('select twice(3)').cost == 4, 'declared call cost plus row produced')
      assert(s.execute("select greet(NULL::text)").rows == [[nil]] && calls.empty?, 'strict NULL skips host kernel')
      assert(s.execute('select greet($1)', 'hello').rows == [['HELLO']], 'host signature infers parameter type')
      error('42883') { s.execute('select twice(3::i32)') }
      s.max_cost = 3
      error('54P01') { s.execute('select twice(3)') }
      s.max_cost = 0
      s.execute('create table t(x i64 default twice(4), check(twice(x)>0))')
      s.execute('insert into t default values')
      assert(s.execute('select x from t').rows == [[8]], 'host default and check')
      error('42P17') { s.execute("create table bad(x text check(greet(x) <> ''))") }
      error('42P17') { s.execute('create index bad_ix on t(greet(x::text))') }
    end
    bad = base.register_function(name: 'badkind', args: [], returns: 'i64') { 'wrong' }
              .register_function(name: 'badwidth', args: [], returns: 'i16') { 1 << 20 }
              .register_function(name: 'batch', args: ['i64'], returns: 'i64', batch_kernel: ->(columns, out) { columns[0].each { |v| out << v + 1 } })
              .register_function(name: 'badbatch', args: ['i64'], returns: 'i64', batch_kernel: ->(_columns, _out) {})
    JedNative.memory(environment: bad) do |core|
      error('22000') { core.execute('select badkind()') }
      error('22003') { core.execute('select badwidth()') }
      error('22000') { core.execute('select badbatch(1)') }
      assert(core.execute('select batch(9)').rows == [[10]], 'column-in batch ABI')
    end
    Dir.mktmpdir('ruby-host-functions') do |directory|
      path = File.join(directory, 'database.jed')
      core = JedNative.create(path, environment: base)
      core.execute_script('create table t(id i64 primary key,v i64 default twice(1), check(twice(v)>0)); insert into t values(1,3); create index t_twice on t(twice(v))')
      core.close
      core = JedNative.open(path, environment: base)
      assert(core.execute('select id from t where twice(v)=6').rows == [[1]], 'matching registry reopens expression index')
      core.close
      core = JedNative.open(path, environment: environment(2))
      assert(core.execute('select id from t').rows == [[1]], 'stale dependency does not prevent reads')
      error('XX002') { core.execute('insert into t values(2,4)') }
      core.execute('reindex index t_twice')
      error('XX002') { core.execute('update t set v=4') }
      core.execute('alter table t drop constraint t_v_check, add constraint fresh check(twice(v)>0)')
      core.execute('insert into t values(2,4)')
      core.close
      core = JedNative.open(path)
      assert(core.execute('select id from t order by id').rows == [[1],[2]], 'missing extension leaves heap readable')
      error('42883') { core.execute('delete from t where id=2') }
      core.execute('drop index t_twice')
      core.execute('delete from t where id=2')
      error('42883') { core.execute('insert into t values(3,5)') }
      core.close
    end
    Dir.mktmpdir('ruby-host-schema') do |directory|
      path = File.join(directory, 'database.jed')
      env = base.register_function(name: 'lim', args: ['i32'], returns: 'i32', component_id: 'test/lim32') { |v| v }
                .register_function(name: 'lim', args: ['i64'], returns: 'i64', component_id: 'test/lim64') { |v| v }
      core = JedNative.create(path, environment: env)
      core.execute_script('create table p(id i64 primary key); insert into p values(1); create table child(id i64 primary key,pid i64 references p on delete cascade,v i64,check(twice(v)>0)); insert into child values(1,1,2); create table t(id i64 primary key,v i32,check(lim(v)<100)); insert into t values(1,3); create table d(id i64 primary key,v i64 default twice(2));')
      core.close
      changed = environment(2).register_function(name: 'lim', args: ['i64'], returns: 'i64', component_id: 'test/lim64') { |v| v }
      core = JedNative.open(path, environment: changed)
      core.execute('alter table child rename column v to value')
      core.execute('alter table child add constraint positive check(value>0)')
      core.execute('delete from p')
      core.execute('alter table t alter column v type i64')
      core.execute('insert into t values(2,4)')
      error('23514') { core.execute('insert into t values(3,100)') }
      core.close
      core = JedNative.open(path)
      core.execute('insert into d values(1,5)')
      error('42883') { core.execute('insert into d(id) select 2 where false') }
      error('42883') { core.execute('update d set v=default where false') }
      core.execute('delete from d')
      core.close
    end
    puts 'ruby-native host functions: strict registry, overloads, metering, batch ABI and persisted identity gates passed'
  end
end
NativeHostFunctionTests.run
