# Opt-in Ruby runtimes

> Historical report: raw evidence and run-specific drivers are on the preserved
> experiment branch. See the [archive and retrieval instructions](ARCHIVE.md).
> Reproduction commands below refer to that original checkout.

The packaged Ruby 4.0.7 used for the original experiment was built with
`--enable-yjit`, without ZJIT. Passing `--zjit` to that executable prints a warning
but exits successfully, so a successful process alone does not prove activation.
Check `RubyVM::ZJIT.enabled?`, as the benchmark harness does.

[Ruby's ZJIT documentation](https://docs.ruby-lang.org/en/4.0/jit/zjit_md.html)
describes the build flag `--enable-zjit` and the runtime flag `--zjit`.
For this follow-up we built the requested
[Ruby 4.1.0-preview1 release](https://www.ruby-lang.org/en/news/2026/10/12/ruby-4-1-0-preview1-released/)
with both JITs. The repository's normal Ruby version and installed mise runtime
remain unchanged. These commands explicitly download and build the optional
runtime under the ignored `.dev/` directory:

```sh
mkdir -p .dev/ruby-native-runtimes/downloads .dev/ruby-native-runtimes/build
curl --fail --location --proto '=https' --tlsv1.2 \
  --output .dev/ruby-native-runtimes/downloads/ruby-4.1.0-preview1.tar.xz \
  https://cache.ruby-lang.org/pub/ruby/4.1/ruby-4.1.0-preview1.tar.xz
ruby -rdigest -e 'abort "source checksum mismatch" unless Digest::SHA256.file(ARGV[0]).hexdigest == "b81f20bf2bc72aa360785eb77ffa0c9f15343ac7316637fbf347500fc3e2e3a8"' \
  .dev/ruby-native-runtimes/downloads/ruby-4.1.0-preview1.tar.xz
tar -xJf .dev/ruby-native-runtimes/downloads/ruby-4.1.0-preview1.tar.xz \
  -C .dev/ruby-native-runtimes/build
ruby_source=.dev/ruby-native-runtimes/build/ruby-4.1.0-preview1
ruby_prefix="$PWD/.dev/ruby-native-runtimes/ruby-4.1.0-preview1"

export JED_NATIVE_MEMORY_BYTES=2147483648
export JED_NATIVE_ADDRESS_SPACE_BYTES=8589934592
export JED_NATIVE_TIMEOUT=1800
mise run ruby-native:bounded -- sh -c \
  'cd "$1" && exec ./configure --prefix="$2" --enable-yjit --enable-zjit --disable-install-doc --without-gmp --with-baseruby="$3"' \
  ruby-configure "$ruby_source" "$ruby_prefix" "$(command -v ruby)"
mise run ruby-native:bounded -- nice -n 10 make -C "$ruby_source" -j2 HAVE_GIT=no
mise run ruby-native:bounded -- make -C "$ruby_source" install HAVE_GIT=no
unset JED_NATIVE_MEMORY_BYTES JED_NATIVE_ADDRESS_SPACE_BYTES JED_NATIVE_TIMEOUT
```

`HAVE_GIT=no` uses the bundled gem archives already present in the release tarball.
Without it, this preview's build attempts to fetch source revisions of bundled
development tools from GitHub. The initial offline attempt stopped at that fetch;
the retry built and installed successfully. The successful build used two jobs
and peaked at 1.11 GiB group RSS, below the 2 GiB build ceiling. These larger build
limits do not change the 512 MiB benchmark worker ceiling.

The shared benchmark harness also needs its existing approved `citrus` 3.0.2 and
`toml-rb` 4.2.0 gems. Both contain only Ruby code. Install their cached archives
offline into the new prefix; do not point `GEM_PATH` at Ruby 4.0's extensions:

```sh
ruby_experiment="$ruby_prefix/bin/ruby"
gem_cache="$(ruby -e 'print Gem.dir')/cache"
experiment_gems="$(env -u GEM_HOME -u GEM_PATH "$ruby_experiment" -e 'print Gem.dir')"
mise run ruby-native:bounded -- env -u RUBYOPT -u GEM_HOME -u GEM_PATH \
  "$ruby_experiment" "$ruby_prefix/bin/gem" install --local --no-document \
  --ignore-dependencies --install-dir "$experiment_gems" \
  "$gem_cache/citrus-3.0.2.gem" "$gem_cache/toml-rb-4.2.0.gem"
env -u RUBYOPT -u GEM_HOME -u GEM_PATH "$ruby_experiment" -Ibench/ruby/lib -rbench -rracc/parser -e \
  'abort "TOML probe failed" unless TomlRB.parse("example = 42\n") == {"example" => 42}; puts $LOADED_FEATURES.grep(/racc.*\.so|date_core\.so/)'
```

The local-only resolver in this preview's RubyGems rejected the already installed
Citrus dependency. The explicit archive installation skips dependency resolution;
the recorded probe verifies the installed requirements (`citrus` 3.0.2 and the
preview's bundled `racc` 1.8.1), successful TOML parsing, and Ruby 4.1 paths for the
Racc and Date native extensions. No new dependency version or network gem fetch
was introduced. Archive hashes and installation attempts are in the runtime
artifact linked below.

Activation and prepared-query correctness can be checked explicitly:

```sh
ruby_experiment="$PWD/.dev/ruby-native-runtimes/ruby-4.1.0-preview1/bin/ruby"
JED_NATIVE_ADDRESS_SPACE_BYTES=2147483648 mise run ruby-native:bounded -- \
  env -u RUBYOPT -u RUBY_YJIT_ENABLE -u RUBY_ZJIT_ENABLE \
  "$ruby_experiment" --disable-yjit --disable-zjit bench/ruby-native/runtime_probe.rb interpreter
JED_NATIVE_ADDRESS_SPACE_BYTES=2147483648 mise run ruby-native:bounded -- \
  env -u RUBYOPT -u RUBY_YJIT_ENABLE -u RUBY_ZJIT_ENABLE \
  "$ruby_experiment" --yjit --yjit-stats=quiet bench/ruby-native/runtime_probe.rb yjit
JED_NATIVE_ADDRESS_SPACE_BYTES=2147483648 mise run ruby-native:bounded -- \
  env -u RUBYOPT -u RUBY_YJIT_ENABLE -u RUBY_ZJIT_ENABLE \
  "$ruby_experiment" --zjit --zjit-stats-quiet bench/ruby-native/runtime_probe.rb zjit
```

Each probe verifies 1,000 prepared primary-key results and checks the requested
mode; JIT probes also require actual compiled methods. Ruby 4.1 uses
`--zjit-stats-quiet`, whereas Ruby 4.0 documentation shows `--zjit-stats=quiet`.
Omit statistics flags from timed benchmark samples. A single process can enable
only one JIT. The checked source digest, build commands, resource measurements,
runtime configuration and activation probes are recorded in
[runtime-build.json](ARCHIVE.md#retrieve-evidence) (archived `bench/ruby-native/results/lookup-followup/runtime-build.json`).
