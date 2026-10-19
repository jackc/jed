# frozen_string_literal: true

module JedNative
  # Kernels may run ahead over a bounded chunk. Replay still charges each call,
  # raises each error, and checks each returned type at its scalar evaluation site.
  module HostBatches
    MISSING_OUTCOME = Object.new.freeze

    def charge(units = 1)
      if (capture = @host_batch_source_charges)
        previous = capture[0]
        if previous.nil?
          capture[0] = units
        elsif previous.is_a?(Array)
          previous << units
        else
          capture[0] = [previous, units]
        end
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
      return nil if unit.zero?
      remaining = @max_cost - @cost if @max_cost.positive?
      if @lifetime_max_cost.positive?
        lifetime_remaining = @lifetime_max_cost - @lifetime_cost
        remaining = lifetime_remaining if !remaining || lifetime_remaining < remaining
      end
      return nil unless remaining
      remaining = 0 if remaining.negative?
      remaining.div(unit) + 1
    end

    def host_batch_rows(input, expressions, pulled: false, remaining: nil, row_at: nil)
      nodes = host_batch_nodes(expressions)
      return input if nodes.empty?
      Enumerator.new do |output|
        size = host_batch_chunk_size(pulled, remaining)
        next if size <= 0
        chunk, charges = [], pulled ? [] : nil
        prior = @host_batch_source_charges
        # A source normally charges zero or one event per row. Retain that
        # event directly; only multiple events need a per-row Array because
        # each individual guard must still run at replay.
        recorded = [nil] if pulled
        @host_batch_source_charges = recorded if pulled
        failure, replay_failed = nil, false
        begin
          # Internal iteration keeps the source on this stack. Enumerator#next
          # would otherwise switch a Fiber for every row, even for an Array.
          input.each do |entry|
            chunk << entry
            if pulled
              charges << recorded[0]
              recorded[0] = nil
            end
            if chunk.length == size
              @host_batch_source_charges = prior
              begin
                host_batch_emit(chunk, charges, nodes, row_at, output)
              rescue StandardError
                replay_failed = true
                raise
              end
              chunk, charges = [], pulled ? [] : nil
              size = host_batch_chunk_size(pulled, remaining)
              break if size <= 0
            end
            @host_batch_source_charges = recorded if pulled
          end
        rescue StandardError => error
          raise if replay_failed
          # A source error follows every row successfully pulled before it.
          # Replay may itself stop earlier, in which case this error is dropped.
          failure = error
        ensure
          @host_batch_source_charges = prior
        end
        host_batch_emit(chunk, charges, nodes, row_at, output) unless chunk.empty?
        host_batch_replay_charge(recorded[0]) if pulled
        raise failure if failure
      end
    end

    def host_batch_replay_charge(recorded)
      if recorded.is_a?(Integer)
        charge(recorded)
      elsif recorded
        recorded.each { |units| charge(units) }
      end
    end

    def host_batch_chunk_size(pulled, remaining)
      size = pulled ? 64 : 1024
      headroom = host_batch_headroom(1) if pulled
      size = headroom if headroom && headroom < size
      available = remaining.call if remaining
      size = available if available && available < size
      size
    end

    def host_batch_emit(chunk, charges, nodes, row_at, output)
      rows = row_at ? chunk.map(&row_at) : chunk
      cache = {}
      nodes.each { |expression| cache[expression.object_id] = host_batch_prefetch(expression, rows) }
      # Outcomes have the same positions as the bounded chunk. Only the row
      # currently being replayed can use them; nested batches retain their own
      # frame. No row-id hash or lookup is needed for every result.
      frame = [cache, nil, 0]
      @host_batch_stack ||= []
      @host_batch_stack << frame
      index = 0
      begin
        while index < chunk.length
          host_batch_replay_charge(charges[index]) if charges
          frame[1], frame[2] = rows[index], index
          output << chunk[index]
          index += 1
        end
      ensure
        @host_batch_stack.pop
      end
    end

    def host_batch_prefetch(expression, rows)
      size = [rows.length, host_batch_headroom(expression.value.cost) || rows.length].min
      arguments = expression.args
      values, selected, columns = Array.new(rows.length, MISSING_OUTCOME), [], Array.new(arguments.length) { [] }
      # Successful values occupy their chunk positions directly. A kernel has
      # at most one failing row, so its error needs no per-row result envelope.
      outcomes = [values, nil, nil]
      index = 0
      while index < size
        row, row_index = rows[index], index
        index += 1
        begin
          # A deferred large value may perform I/O or fail. Leave it to replay.
          unavailable = row.is_a?(PagedRow) && arguments.any? do |argument|
            argument.kind == :column && row.instance_variable_get(:@loaded)[argument.value].zero? &&
              row.storage_leaf.units(row.storage_slot, [argument.value]).any?(&:positive?)
          end
          next if unavailable
          if arguments.length == 1
            argument = arguments[0]
            value = case argument.kind
            when :column then row[argument.value]
            when :literal then argument.value
            when :param then @params[argument.value]
            end
            if value.nil?
              values[row_index] = nil
              next
            end
            columns[0] << HostValues.copy(value)
          else
            arguments_values = arguments.map do |argument|
              case argument.kind
              when :column then row[argument.value]
              when :literal then argument.value
              when :param then @params[argument.value]
              end
            end
            if arguments_values.any?(&:nil?)
              values[row_index] = nil
              next
            end
            arguments_values.each_with_index { |value, index| columns[index] << HostValues.copy(value) }
          end
          selected << row_index
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
        outcomes[1], outcomes[2] = selected.first, Error.new('22000', 'batch kernel does not identify a failing row')
        return outcomes
      end
      answers.each_with_index { |answer, index| values[selected[index]] = HostValues.copy(answer) }
      if answers.length < selected.length
        failure ||= Error.new('22000', 'batch kernel did not answer every row')
        outcomes[1], outcomes[2] = selected[answers.length], failure
      end
      outcomes
    end

    def host_batch_outcome(expression, row)
      stack = @host_batch_stack
      return MISSING_OUTCOME unless stack
      index = stack.length - 1
      while index >= 0
        frame = stack[index]
        if frame[1].equal?(row)
          node = frame[0][expression.object_id]
          if node
            position = frame[2]
            raise node[2] if node[1] == position
            value = node[0][position]
            return value unless value.equal?(MISSING_OUTCOME)
          end
        end
        index -= 1
      end
      MISSING_OUTCOME
    end
  end
  Engine.prepend(HostBatches)
end
