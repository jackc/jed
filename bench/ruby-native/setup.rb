#!/usr/bin/env ruby
# frozen_string_literal: true

# Reuse the canonical dataset generator, with only its file-host coordination
# changed to exclusive ownership: sandbox hosts need not permit Unix sockets.
# A Go overlay leaves the shared benchmark sources untouched.
require 'fileutils'
require 'json'

root = File.expand_path('../..', __dir__)
build = File.join(root, '.dev/ruby-native-bench-build')
data = File.join(root, '.dev/ruby-native-bench-data')
FileUtils.mkdir_p([build, data])
original = File.join(root, 'bench/go/cmd/bench-setup/main.go')
source = File.read(original)
{
  'jed.CreateOptions{Path: path}' => 'jed.CreateOptions{Path: path, Locking: jed.LockingExclusive}',
  'jed.OpenDatabase(path)' => 'jed.OpenDatabaseWithOptions(path, jed.OpenOptions{Locking: jed.LockingExclusive})'
}.each do |before, after|
  raise "canonical generator changed: missing #{before}" unless source.include?(before)

  source = source.sub(before, after)
end
adapted = File.join(build, 'setup-exclusive.go')
File.write(adapted, source)
overlay = File.join(build, 'overlay.json')
File.write(overlay, JSON.generate('Replace' => { original => adapted }))
binary = File.join(build, 'bench-setup')
environment = { 'GOCACHE' => File.join(build, 'cache'), 'GOPROXY' => 'off', 'GOSUMDB' => 'off' }
unless system(environment, 'go', '-C', File.join(root, 'bench/go'), 'build', '-overlay', overlay,
              '-o', binary, './cmd/bench-setup')
  abort 'offline canonical benchmark generator build failed'
end
exec binary, File.join(root, 'bench/corpus'), data, '--engine', 'jed'
