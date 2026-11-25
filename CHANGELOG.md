# Changelog

## 0.1.1 (2026-11-25)

### Speed

The times below come from release builds on an AMD EPYC 9R45 cloud machine, each measured beside 0.1.0 and C QuickJS-ng v0.17.0 on the same machine.

- `let` and `const` in functions run on the fast path, and the compiler drops the checks for use before initialization where it proves them unneeded. A 10-million-step `let` loop in a function went from 1,716 ms to 109 ms. QuickJS-ng takes 161 ms.
- Top-level `let` and `const`, and updates of globals such as `x += 1`, stay on the fast path too. The top-level `let` loop went from 3,916 ms to 197 ms. A top-level `var` loop is still slower than QuickJS-ng, 347 ms against 140 ms.
- An interrupt handler is called about once every 1,000 backward jumps or calls, as QuickJS-ng does, instead of on every one. A handler that reads the clock no longer slows loops, and a stop still ended every test's loop within 0.1 ms in a release build. `for...of` over arrays and the inline built-ins keep their fast paths while a handler is set.
- `JSON.stringify` and `JSON.parse` keep Latin-1 text at one byte a character. A round trip of 100,000 objects went from 188 ms to about 65 ms. QuickJS-ng takes 62 to 78 ms.
- The compiler looks up built-in names by hash and verifies each function once, which halves the time to compile a short script.

### Error messages

- Errors name what went wrong as QuickJS-ng's do, such as "undeclared is not defined", the property that isn't a function and the position of bad JSON. Of 127 common mistakes, 95 now read as in QuickJS-ng, up from 24. A TypeScript annotation reads "Unexpected token ':'".

## 0.1.0 (2026-11-25)

Initial release of Sandbolt, a pure-Rust port of QuickJS-ng v0.17.0 with `#![deny(unsafe_code)]` and no external dependencies. Publishes `sandbolt`, `sandbolt-runtime` and the seven internal compiler, bytecode, syntax, regexp, BigInt, number-formatting and Unicode crates.
