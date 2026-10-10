#!/usr/bin/env ruby
# frozen_string_literal: true

# scripts/npm_ci.rb DIR — install DIR's npm dependencies with `npm ci` when they are missing or
# its package-lock.json changed since the last install (Tasks.npm_ci_if_stale explains the
# content-hash stamp). The shell tasks in mise.toml call this before using a package's tools.

require_relative "lib/tasks"

abort "usage: ruby scripts/npm_ci.rb DIR" unless ARGV.size == 1
Tasks.npm_ci_if_stale(ARGV.first)
