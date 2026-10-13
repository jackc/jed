//! The deterministic-simulation runner binary (spec/design/simulation-testing.md §11). Test-only:
//! built with `--features sim` and driven by scripts/sim.rb; all logic lives in the library's
//! feature-gated `sim` module so it can reach the engine's internals.

fn main() -> std::process::ExitCode {
    jed::sim::main()
}
