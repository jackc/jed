#!/usr/bin/env ruby
# frozen_string_literal: true
require_relative '../../../scripts/lib/bundle_setup'
require 'toml-rb'
root = File.expand_path('../../..', __dir__)
data = TomlRB.load_file(File.join(root, 'spec/cost/schedule.toml'))
lines = ["# frozen_string_literal: true", "# Generated from spec/cost/schedule.toml; opt-in experiment snapshot.", "module JedNative", "  module MemorySchedule"]
data.fetch('memory').each { |key, value| lines << "    #{key.upcase} = #{value}" }
lines.concat(["  end", "end", ""])
File.write(File.join(root, 'impl/ruby-native/lib/jed_native/memory_schedule.rb'), lines.join("\n"))
