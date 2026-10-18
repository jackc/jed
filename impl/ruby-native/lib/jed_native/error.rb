# frozen_string_literal: true

module JedNative
  class Error < StandardError
    attr_reader :sqlstate

    def initialize(sqlstate, message)
      @sqlstate = sqlstate
      super("#{sqlstate}: #{message}")
    end
  end
end
