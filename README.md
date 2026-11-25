# Sandbolt

Sandbolt is a pure-Rust port of [QuickJS-ng](https://github.com/quickjs-ng/quickjs) v0.17.0, a small embeddable JavaScript engine. Every crate builds under `#![deny(unsafe_code)]` with no external dependencies, so a Rust host can run JavaScript without linking a C runtime or carrying a heavy engine. For untrusted scripts, it enforces hard heap and stack caps before allocation, stops execution through an uncatchable interrupt handler, and exposes only the globals and callbacks the host allows.

## Install

```sh
cargo add sandbolt
```

## Example

```rust
use sandbolt::api::{EvalOptions, JsEngine};
use sandbolt::{EngineError, EngineLimits};

fn main() -> Result<(), EngineError> {
    let mut engine = JsEngine::new(EngineLimits {
        heap_bytes: 4 * 1024 * 1024,
        stack_bytes: 256 * 1024,
    })?;

    let sum = engine.eval_to_string(
        "[1, 2, 3].reduce((a, b) => a + b, 0)",
        EvalOptions::default(),
    )?;
    assert_eq!(sum, "6");

    let err = engine.eval_to_string(
        "throw new TypeError('bad input')",
        EvalOptions::default(),
    );
    assert!(matches!(
        err,
        Err(EngineError::Thrown { ref message, .. }) if message == "TypeError: bad input"
    ));

    Ok(())
}
```

Use `set_interrupt_handler` to stop scripts with an uncatchable `EngineError::Interrupted`, and `install_host_bridge` or `set_print_callback` to expose host functions. No filesystem, network or OS globals are installed by default.

## Benchmarks and footprint

The table compares default release builds of Sandbolt (`cargo build --release`, `rustc 1.97.1`) against C QuickJS-ng v0.17.0 (`-O3 -DNDEBUG`). Clang 21.1.8 was the fastest of three C compilers tested, and ratios below 1.000 mean Sandbolt took less time.

| Metric | Sandbolt | C QuickJS-ng v0.17.0 |
|---|---:|---:|
| `microbench.js` median total, 20 rounds (ns) | 4,246.01 | 4,315.15 (Clang 21.1) |
| `microbench.js` geometric mean vs Clang 21.1 (95% CI) | 0.956 (0.954 to 0.958) | 1.000 |
| `microbench.js` total ratio vs Clang 21.1 (95% CI) | 0.985 (0.981 to 0.995) | 1.000 |
| Bootstrap heap used, pre- and post-GC (KiB) | 115.4 | 84.4 |
| RSS per retained post-GC engine, 32 engines (KiB) | 106.0 | not measured |
| Cold engine startup p50, 50 runs (ms) | 0.190 | 0.040 |
| Test262 errors, QuickJS-ng's `test262.conf` (below) | 0 / 82,220 | 49 / 82,220 |
| `unsafe` blocks / external runtime crates | 0 / 0 | C codebase |

The `microbench.js` numbers come from 20 randomized-order paired rounds of all 57 tests timed with `os.now` on an AMD EPYC 9R45 pinned to CPU 0. On that suite, Sandbolt takes less time than C QuickJS-ng in both geometric mean (each test weighted equally) and total time. Against GCC 13.4.0 (`4,523.44` ns median total), Sandbolt's geometric mean is `0.930` (95% CI `0.928` to `0.932`) and total ratio is `0.939` (95% CI `0.935` to `0.946`). Against GCC 15.2.0 (`4,739.62` ns median total), the geometric mean is `0.849` (95% CI `0.846` to `0.853`) and total ratio is `0.897` (95% CI `0.891` to `0.908`). All intervals are exact binomial 95% intervals for the median round.

C stays ahead on cold engine startup and bootstrap heap because Sandbolt builds its initial shapes and intrinsics at boot rather than cloning static tables. On `microbench.js`, Clang 21.1 leads in median time on 28 of the 57 tests (Sandbolt leads on 29), including function calls, closures, regular expressions and `Array.prototype.sort`. These numbers hold for this virtualized Zen 5 host, these default release builds (without LTO or PGO), and QuickJS's microbenchmark suite rather than full application workloads.

## Test262

On `tc39/test262` (`5ef1e57`) using QuickJS-ng's `test262.conf` (`-a -m --phase-strict`), with the same 4,802 excluded and 5,736 skipped tests as upstream, Sandbolt passes all 82,220 executed variants with 0 errors. Upstream C QuickJS-ng v0.17.0 reports 49 errors on the same configuration without `--phase-strict`.

## License

Sandbolt is licensed under the [MIT License](LICENSE), Copyright (c) 2026 Mesa Hills Research. As a port of [QuickJS-ng](https://github.com/quickjs-ng/quickjs), the parts derived from it carry QuickJS-ng's MIT notice ([LICENSE-QUICKJS](LICENSE-QUICKJS)), crediting Fabrice Bellard, Charlie Gordon, Ben Noordhuis, Saúl Ibarra Corretgé and the QuickJS-ng contributors. See [NOTICE](NOTICE) for those attributions and for the Unicode Character Database tables in `sandbolt-unicode`, which are under the Unicode License v3.
