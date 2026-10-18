# frozen_string_literal: true

module JedNative
  # Kernels may run ahead over a bounded chunk. Replay still charges each call,
  # raises each error, and checks each returned type at its scalar evaluation site.
  module HostBatches
    def charge(units = 1)
      if @host_batch_source_charges
        @host_batch_source_charges << units
        return
      end
      super
    end

    def host_batch_nodes(expressions)
      found, stack = [], Array(expressions).compact
      until stack.empty?
        expression = stack.pop
        next unless expression.is_a?(Expression)
        if expression.kind == :host_call
          function = expression.value
          if function.batch_kernel && function.volatility != :volatile && !expression.args.empty? &&
             expression.args.all? { |argument| %i[column literal param].include?(argument.kind) }
            found << expression
          end
        elsif %i[cast unary numeric_context collate].include?(expression.kind) ||
              expression.kind == :binary && !%w[and or].include?(expression.value)
          stack.concat(expression.args || [])
        end
      end
      found.uniq
    end

    def host_batch_headroom(unit)
      remaining = []
      remaining << @max_cost - @cost if @max_cost.positive?
      remaining << @lifetime_max_cost - @lifetime_cost if @lifetime_max_cost.positive?
      return nil if unit.zero? || remaining.empty?
      [remaining.min, 0].max.div(unit) + 1
    end

    def host_batch_rows(input, expressions, pulled: false, remaining: nil, row_at: nil)
      nodes = host_batch_nodes(expressions)
      return input if nodes.empty?
      Enumerator.new do |output|
        source = input.each
        finished = false
        until finished
          size = pulled ? 64 : 1024
          size = [size, host_batch_headroom(1)].min if pulled && host_batch_headroom(1)
          size = [size, remaining.call].min if remaining
          break if size <= 0
          chunk, charges, failure, failure_charges = [], [], nil, []
          size.times do
            prior = @host_batch_source_charges
            recorded = []
            @host_batch_source_charges = recorded if pulled
            begin
              chunk << source.next
              charges << recorded
            rescue StopIteration
              finished = true
              break
            rescue StandardError => error
              failure = error
              failure_charges = recorded
              finished = true
              break
            ensure
              @host_batch_source_charges = prior
            end
          end
          unless chunk.empty?
            rows = row_at ? chunk.map(&row_at) : chunk
            cache = {}
            nodes.each { |expression| cache[expression.object_id] = host_batch_prefetch(expression, rows) }
            @host_batch_stack ||= []
            @host_batch_stack << cache
            begin
              chunk.each_with_index do |entry, index|
                charges[index].each { |units| charge(units) }
                output << entry
              end
            ensure
              @host_batch_stack.pop
            end
          end
          if failure
            failure_charges.each { |units| charge(units) }
            raise failure
          end
        end
      end
    end

    def host_batch_prefetch(expression, rows)
      size = [rows.length, host_batch_headroom(expression.value.cost) || rows.length].min
      outcomes, selected, columns = {}, [], Array.new(expression.args.length) { [] }
      rows.first(size).each do |row|
        begin
        # A deferred large value may perform I/O or fail. Leave it to replay.
        unavailable = row.is_a?(PagedRow) && expression.args.any? do |argument|
          argument.kind == :column && !row.instance_variable_get(:@loaded)[argument.value] &&
            row.storage_leaf.units(row.storage_slot, [argument.value]).any?(&:positive?)
        end
        next if unavailable
        values = expression.args.map do |argument|
          case argument.kind
          when :column then row[argument.value]
          when :literal then argument.value
          when :param then @params[argument.value]
          end
        end
        if values.any?(&:nil?)
          outcomes[row.object_id] = [false, nil]
          next
        end
        selected << row
        values.each_with_index { |value, index| columns[index] << HostValues.copy(value) }
        rescue Error
          # Decoding faults retain the normal argument evaluation/guard order.
          next
        end
      end
      return outcomes if selected.empty?
      answers, failure = [], nil
      begin
        expression.value.batch_kernel.call(columns, answers)
      rescue StandardError => error
        failure = error
      end
      if answers.length > selected.length || failure && answers.length == selected.length
        outcomes[selected.first.object_id] = [true, Error.new('22000', 'batch kernel does not identify a failing row')]
        return outcomes
      end
      answers.each_with_index { |answer, index| outcomes[selected[index].object_id] = [false, HostValues.copy(answer)] }
      if answers.length < selected.length
        failure ||= Error.new('22000', 'batch kernel did not answer every row')
        outcomes[selected[answers.length].object_id] = [true, failure]
      end
      outcomes
    end

    def host_batch_outcome(expression, row)
      (@host_batch_stack || []).reverse_each do |cache|
        node = cache[expression.object_id]
        return node[row.object_id] if node&.key?(row.object_id)
      end
      nil
    end
  end
  Engine.prepend(HostBatches)
end
