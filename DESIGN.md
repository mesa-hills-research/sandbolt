# Pure-Rust Port of QuickJS-ng v0.17.0 — Architecture Plan

## 1. Goal & Scope

- **Upstream baseline**: `quickjs-ng/quickjs` `v0.17.0` (`6d46d07d04041b40f4f49eaa7fdebe44c314c699`, 2026-09-18). Reference C checkout and compiled oracle binaries live outside this repo at `../quickjs-ref`.
- **In scope**: Core ECMAScript engine (`quickjs`, `libregexp`, `libunicode`, `dtoa`, `BigInt`, ES2025/2026 features in v0.17.0, bytecode serialization) + pure-Rust parallel `test262` runner (`tc39/test262` pinned at `5ef1e5723be95296f36afb0386676fed0205869c`).
- **Out of scope for v1**: `quickjs-libc` (`std`/`os` I/O), `Intl`, `Temporal`, `ShadowRealm`, `decorators`, `tail-call-optimization`, `Atomics.waitAsync`, `import-defer`, `source-phase-imports`.
- **Dependencies**: Zero external runtime crates in the engine workspace (all core subsystems ported directly in pure Rust).
- **Safety policy**: Safe Rust by default (`#![deny(unsafe_code)]` across the workspace during parity). Narrow, audited `unsafe` is considered only in Phase 6 if benchmarks prove a real gap.
- **Sequencing**: Parity first, optimization second.
  - **Parity bar**:
    1. Differential parity against C `quickjs-ng v0.17.0` on `libunicode`, `dtoa`, `BigInt`, `libregexp`, and QuickJS-ng's core `tests/` suite (116/116 in C, minus `std`/`os`-only tests).
    2. Full Arko `code_mode` acceptance parity (`EngineLimits` heap/stack caps, uncatchable `stop` interrupt across loops/try-finally/regex/await, catchable OOM `InternalError("out of memory")`, `__host` bridge, strict-mode eval option, exact `globalThis` surface including `DOMException`/`performance`/`atob`/`btoa`/`queueMicrotask`, accurate file/line stack frames).
    3. Full `test262` suite (`-m -c test262.conf -a`), matching upstream first and then driving failures down to **0** (fixing upstream's 49 known mode-rows / 35 test files as well), verified both with upstream-style matching and phase-accurate negative error checking.
    4. Clean passes under `gc_stress` (GC on every allocation/reentry) and OOM failpoint injection.
  - **Optimization round (Phase 6)**: Profiling, VM dispatch tuning, inline caches, and SIMD/string/array fast paths against C `quickjs-ng v0.17.0` on a quiet bare-metal box.

---

## 2. Core Architectural Decisions

### 2.1 Memory Management, GC, and Rooting

- **Collector**: Precise stop-the-world tracing Mark-and-Sweep over typed, non-moving index arenas (`Arena<JsObject>`, `Arena<JsString>`, `Arena<Shape>`, `Arena<SymbolData>`, `Arena<BigIntData>`, `Arena<BytecodeFunc>`, `Arena<ModuleRecord>`, `Arena<VarRef>`, `Arena<Accessor>`, `Arena<CoroutineState>`, `Arena<Realm>`).
  - Avoids manual refcounting leaks/underflows across parallel workers and avoids `Rc<RefCell<_>>` re-entrancy panics.
  - Naturally supports cycles, `WeakMap` ephemeron fixpoint marking, `WeakRef` (`KeepDuringJob` / `ClearKeptObjects` at turn boundaries), `FinalizationRegistry` (queued cleanup jobs, never executing JS inside GC), multi-realm (`$262.createRealm`), and future Jupyter-like heap snapshotting (flat index tables + symbolic `NativeFnId(u16)` instead of raw function pointers).
- **Rooting discipline**:
  - The VM operand stack (`vm.stack`) serves as the root stack.
  - Native builtins receive `this` and `args` directly as lifetime-branded `Handle<'s, Value>` and `Args<'s>` indexing into `vm.stack`, truncated automatically on return (`RootScope`), with `ctx.with_scope(|ctx| ...)` and `MutRoot<'s, Value>` for loops.
  - **Type-level rooting barrier**: Every allocating or JS-reentrant abstract operation accepts `Handle<'s, _>`, never raw `Value`. Pure read-only inspection takes short `&Heap` / `NoGc` leases.
  - **Deterministic bug detection**: Every GC handle packs a 16-bit generation alongside its 31-bit arena slot index (retiring a slot if generation wraps). Under `--features gc_stress`, the heap runs full mark-and-sweep at configurable intervals (default every 1 allocation in unit/builtin tests), immediately panicking with a backtrace if any unrooted `Value` survives across an allocation.
- **Hard `heap_bytes` cap**:
  - Centralized fallible `HeapLedger` charges owned backing capacity (arena pages, `TrackedVec` backing stores including both buffers during growth, strings, shapes, atoms, AST/bytecode, regex/BigInt scratch buffers, stacks, jobs) *before* allocating.
  - On breach, triggers GC at permitted safepoints and retries once; if still over cap, returns OOM without growing host RSS.
  - Reserves small pre-budgeted headroom for GC mark stacks and OOM `InternalError("out of memory")` construction, tagged off-heap so the host distinguishes OOM reliably even if JS mutates error messages.

### 2.2 Value, String, Atom, Shape, and Object Layout

- **8-byte safe NaN-boxed `Value(u64)`** (zero `unsafe`, via `f64::to_bits`/`from_bits`):
  - All incoming `f64::is_nan()` values are canonicalized to positive qNaN `0x7FF8_0000_0000_0000` (raw `ArrayBuffer` byte copies preserve bit patterns).
  - `bits < 0xFFF8_0000_0000_0000`: IEEE-754 `f64`.
  - `bits >= 0xFFF8_0000_0000_0000`: 4-bit tag (bits `50..47`) + 47-bit payload:
    - Tags `0..7` (immediates): `0 = Int32(i32)` (`is_number` is one compare `bits < 0xFFF8_8000_0000_0000`), `1 = Bool`, `2 = Null`, `3 = Undefined`, `4 = Uninitialized` (TDZ), `5 = Empty` (array hole), `6 = ShortBigInt(i47)`, `7 = CatchOffset(u32)`.
    - Tags `8..15` (GC handles, `is_gc` is one compare `bits >= 0xFFFC_0000_0000_0000`, packing `idx: u31 | (gen: u16 << 31)`): `8 = Object`, `9 = String`, `10 = Symbol`, `11 = BigInt`, `12 = Bytecode`, `13 = Module`, `14 = VarRef`, `15 = Accessor`.
  - Exhaustive matching via `val.decode() -> ValueKind`.
- **Strings & Lossless SourceText**:
  - `JsString` stores flat Latin-1 (`TrackedVec<u8>` + `is_ascii` bit) or UTF-16 (`TrackedVec<u16>`, preserving unpaired surrogates), plus bounded-depth `Rope { left, right, len, is_wide, depth }` flattened iteratively in place.
  - Parser input is lossless `SourceText` (supporting lone surrogates from `eval` / `new Function` / `RegExp`, not restricted to UTF-8 `&str`), with byte spans and a binary-searched `line_starts` table matching QuickJS's UTF-8 byte column convention.
- **Property Keys & Atoms**:
  - `PropKey(u32)` encodes inline small array indices (`0..2^30-1`), interned string `Atom`s (immortal built-in atoms from `quickjs-atom.h` + weakly-interned dynamic string atoms swept by GC), and GC-able `SymId`s (agent-wide well-known + `Symbol.for` registry vs GC-able non-registered symbols eligible as weak keys).
- **Objects, Shapes, Fast Arrays**:
  - `JsObject { shape: ShapeId, props: TrackedVec<Value>, flags: ObjectFlags, data: ObjectData }`.
  - Accessors store `Value::accessor(AccId)` inside flat 8-byte `props`.
  - Shapes share `(proto, prop descriptors)` via weak transition/hash tables and switch to unshared dictionary mode on heavy deletion or >1024 properties.
  - Fast arrays store packed `TrackedVec<Value>` with `Value::EMPTY` holes, downgrading to sparse properties on large sparse writes or non-default attributes, with a per-realm `std_array_prototype` invalidation flag.
  - Exhaustive `ObjectData` enum pre-declares all ~30 boxed builtin payload types in Phase 1 so builtin workers edit disjoint files while compile-time exhaustiveness enforces GC tracing and future snapshot coverage.

### 2.3 Parser & Compiler Pipeline

Clean multi-stage pipeline (replacing QuickJS's single-pass bytecode-rewinding parser):
1. **Lexer (`qjs-syntax`)**: Contextual lexical goals (regex vs div, template tails, hashbang, Unicode IDs via `qjs-unicode`, lossless lone surrogates, raw/cooked literals, exact byte spans).
2. **Parser + Cover Grammar + Early-Error Validator (`qjs-syntax`)**: Arena-backed AST (charged to `heap_bytes`, recursion-bounded against `stack_bytes`, freed after compilation). Pure AST cover-grammar reinterpretation for arrows, async arrows, and destructuring targets; full ES2025 static semantics and strict/Annex B early errors before emitting bytecode.
3. **Scoped IR Emitter (`qjs-compiler`)**: Lowers validated AST to typed `Vec<IrOp>` modeling ES `Reference` evaluation order directly (fixing QuickJS-ng's destructuring/computed-property `ToPropertyKey` ordering and `get`/`set` ASI failures by construction).
4. **Variable & Closure Resolver (`qjs-compiler`)**: Ports QuickJS's scope resolution (`resolve_variables`): lexical TDZ, `var`/Annex B B.3.3 hoisting, `for (let)` per-iteration closure cloning, `with` (`Symbol.unscopables` + `HasBinding` recheck), direct `eval` (dynamic scope + retained var names), sloppy mapped `arguments`, private fields/brands, `using`/`await using` disposal scopes.
5. **Assembler & Verifier (`qjs-compiler` / `qjs-bytecode`)**: Resolves labels, performs short-opcode/jump shrinking, computes exact `stack_size`, builds `pc2line` debug info, and runs a structural bytecode + handler-stack verifier.

### 2.4 VM Execution, Coroutines, Unwinding, and Limits

- **Iterative Trampoline**: Explicit heap-allocated `vm.stack: TrackedVec<Value>` and `vm.frames: TrackedVec<CallFrame>`. Normal JS-to-JS calls, `Function.prototype.call`/`apply`, and `Reflect.apply`/`construct` push/pop `CallFrame`s without consuming Rust call stack. Native-to-JS callbacks (`Proxy`, getters, `Array.sort`, `JSON` reviver, `ToPrimitive`) re-enter `vm.run_until(depth)` with a bounded `reentry_depth`.
- **Deterministic `stack_bytes`**: Checked uniformly from `vm.stack` bytes + `vm.frames` bytes + `reentry_depth * REENTRY_COST` across OSes and debug/release builds.
- **Unified Coroutines**: Sync generators, async functions, and async generators share `Arena<CoroutineState>`. Captured variables use `VarRefState::{OpenStack(u32), OpenCoro { coro, local }, Closed(Value)}` — zero raw stack pointers, GC-safe across suspension and sloppy mapped `arguments`.
- **Unwinding & Interrupts**:
  - `Abrupt::Thrown` (catchable, including OOM `InternalError("out of memory")`): unwinds to `CatchOffset`, executes `finally` and `using` disposers, converts to Promise rejection in async frames.
  - `Abrupt::Interrupted` (uncatchable): bypasses all `catch`, `finally`, `using`, and Promise wrappers straight to `EngineError::Interrupted`.
  - Interrupts (`stop()`), `heap_bytes`, and `stack_bytes` are enforced in VM backedges/calls/resumes, `qjs-regexp` backtracking (`RegExpHost`), parser recursion, `qjs-bigint`, and long builtin loops (`String.repeat/pad`, `Array.fill/sort`, `JSON`).
- **Multi-realm & `$262.agent`**: One `Runtime` per agent (non-`Send` heap); `$262.createRealm()` creates a new `Realm` inside the same `Runtime` with its own intrinsics and `globalThis`; every bytecode and native function carries its `RealmId`. `$262.agent` spawns worker threads with their own `Runtime`s sharing only `Arc<SharedArrayBufferBacking>` (atomic storage + wait/notify table; locks are never held across JS execution).

---

## 3. Workspace Layout & Parallel Swarm Execution Plan

```text
quickjs-port/
├── Cargo.toml                  # Workspace (edition 2024, zero external runtime deps)
├── DESIGN.md
└── crates/
    ├── qjs-unicode/            # Port of libunicode.c + libunicode-table.h
    ├── qjs-dtoa/               # Port of dtoa.c (ES Number <-> string)
    ├── qjs-bigint/             # Port of QuickJS-ng BigInt (charge + interrupt host)
    ├── qjs-regexp/             # Port of libregexp.c (ES2025 /v /d modifiers, RegExpHost)
    ├── qjs-syntax/             # SourceText, lexer, arena AST, early-error validator
    ├── qjs-bytecode/           # Opcodes, IrOp, FunctionBytecode, DebugInfo, verifier, codec
    ├── qjs-compiler/           # AST -> scoped IR -> resolve_vars -> resolve_labels -> Bytecode
    ├── qjs-runtime/            # Unified GC/heap/rooting/objects/ops/VM/coroutines/modules/builtins/API
    └── qjs-test262/            # Parallel test262 runner ($262 harness + differential oracle mode)
```

### Staged Phases (Sol 6.2 xhigh Swarm + Occasional Neon/Astra Advisor Reviews)

- **Phase 1 (Freeze Core Seams & Vertical Slice)**:
  - Build the workspace skeleton and freeze `qjs-bytecode` schemas, `RegExpHost`/`BigIntHost`, and `qjs-runtime`'s core memory/object/rooting/ops/VM seam (`Value`, `Heap`, `HeapLedger`, `Arena`, `RootScope`, `Handle`, `JsString`, `Atom`, `Shape`, `JsObject`, pre-declared `ObjectData`, `Realm`, `NativeFnId`, `CallFrame`).
  - Land a minimal working vertical slice that evaluates nested re-entrant callbacks under `gc_stress` + OOM failpoints, unwinds exceptions & interrupts, runs a Promise job, and checks cross-realm identity.
- **Phase 2 (Parallel Foundation & Frontend Fan-Out — 6 concurrent workers)**:
  - W1: `qjs-unicode` (+ differential test suite against C `libunicode`)
  - W2: `qjs-dtoa` (+ differential test suite against C `dtoa`)
  - W3: `qjs-bigint` (+ differential test suite against C `quickjs` BigInt)
  - W4: `qjs-regexp` (+ differential test suite against C `lre-test` / `libregexp`)
  - W5: `qjs-syntax` (full ES2025 lexer, parser, cover grammar, early errors)
  - W6: `qjs-runtime` core object model, abstract operations (`ops/*`), and `Realm` bootstrap
- **Phase 3 (Compiler, VM, Coroutines, Modules & Harness — 4 concurrent workers)**:
  - W1: `qjs-compiler` (AST → scoped IR → `resolve_vars` → `resolve_labels` → bytecode)
  - W2: `qjs-runtime::vm` (full opcode dispatch, frames, unwinding, exceptions, `eval`, `with`, interrupts, stack limits)
  - W3: `qjs-runtime::vm::{coroutine, job, module}` (generators, async, async generators, Promises/microtasks, ES2025 TLA module graph)
  - W4: `qjs-test262` parallel runner (`$262` harness, `createRealm`, `gc`, `detachArrayBuffer`, `IsHTMLDDA`, `agent`, differential C comparison)
- **Phase 4 (Parallel Builtins & Public API Fan-Out — 7-8 concurrent workers on disjoint files)**:
  - W1: `Object`, `Function`, `Error`/`AggregateError`/`SuppressedError`, `Reflect`, `Proxy`
  - W2: `Array`, `Iterator` helpers (+ `Iterator.zip`/`zipKeyed`, `Array.fromAsync`), `DisposableStack`/`AsyncDisposableStack`
  - W3: `String`, `RegExp`, URI functions (`encodeURI*`/`decodeURI*`/`escape`/`unescape`)
  - W4: `Number`, `Boolean`, `Math`, `BigInt`, `Date`
  - W5: `Map`, `Set`, `WeakMap`, `WeakSet`, `WeakRef`, `FinalizationRegistry`, `Symbol`
  - W6: `ArrayBuffer`, `SharedArrayBuffer`, `TypedArray` (including `Float16Array` + base64/hex), `DataView`, `Atomics`
  - W7: `Promise`, `JSON`, QuickJS-ng globals (`DOMException`, `performance`, `atob`, `btoa`, `queueMicrotask`), bytecode serialization codec
  - W8: Public idiomatic Rust API (`qjs-runtime::api`), Arko `code_mode::Interpreter` acceptance test suite, and snapshot-hook invariant tests
- **Phase 5 (Conformance Drive to 0 Errors)**:
  - Run QuickJS-ng core `tests/`, Arko `code_mode` acceptance tests, `gc_stress`, OOM failpoints, and full `test262` (`-m -c test262.conf -a`).
  - Cluster failures by root cause, dispatch parallel Sol 6.2 xhigh fix workers + `review-sol62` audits (consulting Neon + Astra on genuinely subtle spec conflicts), and drive `test262` down to **0 errors** (including fixing upstream's 49 known failures).
- **Phase 6 (Post-Parity Optimization Round)**:
  - Quiet bare-metal benchmark box comparison vs C `quickjs-ng v0.17.0` (`microbench.js`, `bench-v8`, memory footprint), profiling, and targeted speedups (dispatch, inline caches, string/array/SIMD fast paths) with zero conformance regressions.
