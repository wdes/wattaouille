# Agents

Read CLAUDE.md first if it exists.

## Project

Wattaouille is a live CPU and energy monitor for Linux laptops. It shows what's spinning your fan and how many watts it's burning. Pure Rust CLI tool with no runtime dependencies beyond libc.

## Architecture

- `src/lib.rs` — core types and functions: `Sample`, `PowerSensor`, `snapshot()`, `num_cpus()`, `total_cpu_jiffies()`, proc parsing
- `src/main.rs` — TUI rendering, process labelling (`pretty_cmdline`), energy accounting, signal handling, alt-screen management

## Constraints

- Targets Linux only. Reads from `/proc` and `/sys` directly.
- Uses unsafe code for signal handling and terminal raw mode (libc FFI). These functions have `#[allow(unsafe_code)]` annotations.
- No external runtime dependencies — just a static binary + libc.

## Code style

- Strict clippy: `all`, `pedantic`, `nursery` at deny level. See `Cargo.toml` `[lints.clippy]` for specific allows.
- rustfmt: edition 2024, max_width 120, use_field_init_shorthand true.
- License header: MPL-2.0 comment at top of each source file.
- Minimal comments — only when the why is non-obvious.
- No unnecessary abstractions.

## Testing

- 58 tests (12 lib + 46 bin). Run with `cargo test --all-targets`.
- CI includes an integration test: builds `--release`, uploads artifact, then runs the binary inside `debian:trixie-slim` to verify runtime deps.
