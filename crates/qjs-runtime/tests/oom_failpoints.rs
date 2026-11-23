#![deny(unsafe_code)]

use std::sync::atomic::{AtomicUsize, Ordering};

use qjs_runtime::api::{EvalOptions, JsEngine, ModuleLoader, ResolvedModule};
use qjs_runtime::ledger::HeapLedger;
use qjs_runtime::{EngineError, EngineLimits};

struct FixtureLoader;

impl ModuleLoader for FixtureLoader {
    fn resolve(
        &mut self,
        _: &str,
        specifier: &str,
        module_type: Option<&str>,
    ) -> Result<ResolvedModule, String> {
        match (specifier, module_type) {
            ("dep", None) => Ok(ResolvedModule::Source {
                name: "dep".into(),
                source: "import { second } from 'leaf'; export const n = await Promise.resolve(second + 1);".into(),
            }),
            ("leaf", None) => Ok(ResolvedModule::Source {
                name: "leaf".into(),
                source: "export const second = await Promise.resolve(40);".into(),
            }),
            ("simple", None) => Ok(ResolvedModule::Source {
                name: "simple".into(),
                source: "export const n = 41;".into(),
            }),
            ("data.json", Some("text")) => Ok(ResolvedModule::Text {
                name: "data.json".into(),
                text: "{\"v\":41}".into(),
            }),
            ("data.json", Some("json") | None) => Ok(ResolvedModule::Json {
                name: "data.json".into(),
                source: "{\"v\":41}".into(),
            }),
            _ => Err(format!("missing fixture module '{specifier}'")),
        }
    }
}

fn bare_ledger_bytes(limits: EngineLimits) -> usize {
    HeapLedger::new(limits.heap_bytes, (limits.heap_bytes / 32).min(4096))
        .unwrap()
        .used_bytes()
}

fn assert_clean_drop(engine: JsEngine, ledger: &HeapLedger, bare: usize, label: &str, k: usize) {
    drop(engine);
    assert_eq!(
        ledger.used_bytes(),
        bare,
        "{label} failpoint {k}: HeapLedger leaked {} bytes on drop",
        ledger.used_bytes() as isize - bare as isize
    );
}

#[derive(Clone, Copy)]
enum OomOutcome {
    /// Uncaught OOM must surface as `Err(EngineError::OutOfMemory)`.
    Strict,
    /// User JS `try/catch` catches OOM during the inner step and returns `catch_value`.
    OrCaught(&'static str),
    /// User JS promise rejection handler catches OOM in async jobs (`result` holds `caught_result`).
    OrAsyncCaught(&'static str),
    /// The script intentionally throws a non-OOM JS value; OOM may strike either
    /// before the throw (yielding `OutOfMemory`) or while formatting the already-thrown
    /// exception (gracefully degrading to `EngineError::Thrown`).
    OrThrownFallback,
}

#[derive(Clone, Copy)]
struct SweepCase {
    name: &'static str,
    setup: &'static str,
    source: &'static str,
    expected: &'static str,
    jobs: bool,
    module: bool,
    outcome: OomOutcome,
}

fn run_sweep(case: SweepCase) {
    let limits = EngineLimits::default();
    let bare = bare_ledger_bytes(limits);
    let base_opts = EvalOptions::default();

    // First verify baseline succeeds without any failpoint and discover how many
    // failpoints the operation exercises.
    let mut baseline = JsEngine::new(limits).unwrap();
    baseline.set_module_loader(Box::new(FixtureLoader));
    if !case.setup.is_empty() {
        baseline.eval(case.setup, base_opts.clone()).unwrap();
    }
    baseline.collect_garbage();
    let mut eval_opts = base_opts.clone();
    if case.module {
        eval_opts.module = true;
        eval_opts.filename = "entry".into();
    }
    let baseline_ledger = baseline.runtime().heap().ledger.clone();
    let base_res = if case.module {
        baseline
            .eval(case.source, eval_opts.clone())
            .map(|()| String::new())
            .and_then(|s| {
                if case.jobs {
                    baseline.run_jobs()?;
                }
                Ok(s)
            })
    } else {
        baseline
            .eval_to_string(case.source, eval_opts.clone())
            .and_then(|s| {
                if case.jobs {
                    baseline.run_jobs()?;
                }
                Ok(s)
            })
    };
    match case.outcome {
        OomOutcome::OrThrownFallback => {
            assert!(
                matches!(base_res, Err(EngineError::Thrown { .. })),
                "{} baseline expected Thrown, got {base_res:?}",
                case.name
            );
        }
        _ => {
            assert_eq!(
                base_res,
                Ok(case.expected.into()),
                "{} baseline failed",
                case.name
            );
            if case.jobs || case.module {
                let var = if case.name.contains("finalization") {
                    "cleanup"
                } else {
                    "result"
                };
                let expected_val = if case.name.contains("finalization") {
                    "9"
                } else {
                    "42"
                };
                assert_eq!(
                    baseline.eval_to_string(var, base_opts.clone()),
                    Ok(expected_val.into()),
                    "{} baseline {var} mismatch",
                    case.name
                );
            }
        }
    }
    assert_clean_drop(baseline, &baseline_ledger, bare, case.name, usize::MAX);

    let mut fired_any = false;
    for k in 0..=4096 {
        let mut engine = JsEngine::new(limits).unwrap();
        engine.set_module_loader(Box::new(FixtureLoader));
        if !case.setup.is_empty() {
            engine.eval(case.setup, base_opts.clone()).unwrap();
        }
        engine.collect_garbage();
        let ledger = engine.runtime().heap().ledger.clone();
        ledger.fail_after(Some(k));

        let res = if case.module {
            engine
                .eval(case.source, eval_opts.clone())
                .map(|()| String::new())
                .and_then(|s| {
                    if case.jobs {
                        engine.run_jobs()?;
                    }
                    Ok(s)
                })
        } else {
            engine
                .eval_to_string(case.source, eval_opts.clone())
                .and_then(|s| {
                    if case.jobs {
                        engine.run_jobs()?;
                    }
                    Ok(s)
                })
        };

        let fired = !ledger.failpoint_armed();
        ledger.fail_after(None);

        if !fired {
            match case.outcome {
                OomOutcome::OrThrownFallback => {
                    assert!(matches!(res, Err(EngineError::Thrown { .. })));
                }
                _ => assert_eq!(res, Ok(case.expected.into())),
            }
            assert_clean_drop(engine, &ledger, bare, case.name, k);
            assert!(fired_any, "{} never triggered a failpoint", case.name);
            return;
        }
        fired_any = true;

        match case.outcome {
            OomOutcome::Strict => {
                assert_eq!(
                    res,
                    Err(EngineError::OutOfMemory),
                    "{} failpoint {k}: expected OutOfMemory, got {res:?}",
                    case.name
                );
            }
            OomOutcome::OrCaught(caught) => match &res {
                Err(EngineError::OutOfMemory) => {}
                Ok(val) if val == caught => {}
                other => panic!(
                    "{} failpoint {k}: expected OutOfMemory or caught {caught:?}, got {other:?}",
                    case.name
                ),
            },
            OomOutcome::OrAsyncCaught(caught) => match &res {
                Err(EngineError::OutOfMemory) => {}
                Ok(val) if val == case.expected => {
                    let result = engine.eval_to_string("result", base_opts.clone());
                    assert_eq!(
                        result,
                        Ok(caught.into()),
                        "{} failpoint {k}: async rejection handler should have caught OOM",
                        case.name
                    );
                }
                other => panic!(
                    "{} failpoint {k}: expected OutOfMemory or async-caught {caught:?}, got {other:?}",
                    case.name
                ),
            },
            OomOutcome::OrThrownFallback => match &res {
                Err(EngineError::OutOfMemory) | Err(EngineError::Thrown { .. }) => {}
                other => panic!(
                    "{} failpoint {k}: expected OutOfMemory or Thrown fallback, got {other:?}",
                    case.name
                ),
            },
        }

        // VM must unwind completely with clean stack/frames/reentry depth.
        let vm = &engine.runtime().vm;
        assert_eq!(
            (vm.stack_len(), vm.frame_count(), vm.reentry_depth()),
            (0, 0, 0),
            "{} failpoint {k}: dirty VM state after OOM",
            case.name
        );

        // Engine must survive GC and remain usable for subsequent evaluations.
        engine.collect_garbage();
        assert_eq!(
            engine.eval_to_string("40 + 2", base_opts.clone()),
            Ok("42".into()),
            "{} failpoint {k}: engine broken after OOM",
            case.name
        );

        // Retrying a failed module on the recovered engine must never panic or
        // surface corrupted module state (only clean success or cached OOM).
        if case.module {
            let retry = engine.eval(case.source, eval_opts.clone());
            let jobs = engine.run_jobs();
            let value = engine.eval_to_string("result", base_opts.clone());
            match (&retry, &jobs, &value) {
                (Ok(()), Ok(()), Ok(v)) if v == "42" => {}
                (Err(EngineError::OutOfMemory), Ok(()), Ok(v)) if v == "0" => {}
                (Ok(()), Err(EngineError::OutOfMemory), Ok(v)) if v == "0" => {}
                other => panic!(
                    "{} failpoint {k}: unexpected module retry state {other:?}",
                    case.name
                ),
            }
        }

        assert_clean_drop(engine, &ledger, bare, case.name, k);
    }
    panic!("{} exceeded 4096 failpoints without completing", case.name);
}

#[test]
fn bootstrap_oom_failpoints() {
    let limits = EngineLimits::default();
    let bare = bare_ledger_bytes(limits);

    // Find the total charge count for `JsEngine::new` via binary search.
    let mut lo = 0usize;
    let mut hi = 16_384usize;
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let ledger =
            HeapLedger::new(limits.heap_bytes, (limits.heap_bytes / 32).min(4096)).unwrap();
        ledger.fail_after(Some(mid));
        let res = JsEngine::new_with_ledger(limits, ledger.clone());
        let fired = !ledger.failpoint_armed();
        ledger.fail_after(None);
        drop(res);
        assert_eq!(ledger.used_bytes(), bare);
        if fired {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    let total = lo;
    assert!(total > 100, "expected substantial bootstrap charges, got {total}");

    let workers = std::thread::available_parallelism()
        .map_or(4, |n| n.get())
        .min(8);
    let next = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| loop {
                let k = next.fetch_add(1, Ordering::Relaxed);
                if k >= total {
                    break;
                }
                let ledger =
                    HeapLedger::new(limits.heap_bytes, (limits.heap_bytes / 32).min(4096))
                        .unwrap();
                ledger.fail_after(Some(k));
                let res = JsEngine::new_with_ledger(limits, ledger.clone());
                assert!(
                    !ledger.failpoint_armed(),
                    "bootstrap failpoint {k}/{total} did not fire"
                );
                ledger.fail_after(None);
                assert!(
                    matches!(res, Err(EngineError::OutOfMemory)),
                    "bootstrap failpoint {k}/{total}: expected OutOfMemory, got {:?}",
                    res.err()
                );
                drop(res);
                assert_eq!(
                    ledger.used_bytes(),
                    bare,
                    "bootstrap failpoint {k}/{total} leaked {} bytes",
                    ledger.used_bytes() as isize - bare as isize
                );
            });
        }
    });
}

#[test]
fn secondary_realm_oom_failpoints() {
    let limits = EngineLimits::default();
    let bare = bare_ledger_bytes(limits);

    let probe_once = |k: usize| -> bool {
        let mut engine = JsEngine::new(limits).unwrap();
        let ledger = engine.runtime().heap().ledger.clone();
        let before_realms = engine.runtime().heap().realms.len();
        ledger.fail_after(Some(k));
        let realm = engine.default_realm();
        let res = engine
            .runtime_mut()
            .with_scope(realm, |ctx| ctx.create_realm_with_builtins());
        let is_oom = engine.runtime().is_oom();
        let fired = !ledger.failpoint_armed();
        ledger.fail_after(None);
        if !fired {
            assert!(res.is_ok(), "secondary realm {k} failed without failpoint");
            assert_clean_drop(engine, &ledger, bare, "secondary_realm", k);
            return false;
        }
        assert!(
            res.is_err() && is_oom,
            "secondary realm failpoint {k}: expected OOM error (is_oom={is_oom}, res={res:?})"
        );
        // Clear pending exception before GC so the failed realm has no live references.
        engine.runtime_mut().with_scope(realm, |ctx| {
            ctx.take_exception();
        });
        engine.collect_garbage();
        assert_eq!(
            engine.runtime().heap().realms.len(),
            before_realms,
            "secondary realm failpoint {k}: failed realm remained rooted after GC"
        );
        assert_eq!(
            engine.eval_to_string("40 + 2", EvalOptions::default()),
            Ok("42".into()),
            "secondary realm failpoint {k}: host engine broken after failed realm creation"
        );
        assert_clean_drop(engine, &ledger, bare, "secondary_realm", k);
        true
    };

    let mut lo = 0usize;
    let mut hi = 16_384usize;
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if probe_once(mid) {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    let total = lo;
    assert!(total > 100, "expected substantial secondary realm charges, got {total}");

    let workers = std::thread::available_parallelism()
        .map_or(4, |n| n.get())
        .min(8);
    let next = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| loop {
                let k = next.fetch_add(1, Ordering::Relaxed);
                if k >= total {
                    break;
                }
                assert!(
                    probe_once(k),
                    "secondary realm failpoint {k}/{total} unexpectedly did not fire"
                );
            });
        }
    });
}

#[test]
fn parser_compiler_literals_oom() {
    run_sweep(SweepCase {
        name: "parser_compiler_literals",
        setup: "",
        source: "(() => { const a = [1, /x+/g, 'wide文字']; class C { #x = a[2]; get x(){return this.#x} } return `${new C().x}:${a[1].test('x')}` })()",
        expected: "wide文字:true",
        jobs: false,
        module: false,
        outcome: OomOutcome::Strict,
    });
}

#[test]
fn catchable_oom_recovery() {
    run_sweep(SweepCase {
        name: "catchable_oom_recovery",
        setup: "",
        source: "(() => { try { return new Array(36).fill(1).slice(1).length } catch (e) { return e.name + ':' + e.message } })()",
        expected: "35",
        jobs: false,
        module: false,
        outcome: OomOutcome::OrCaught("InternalError:out of memory"),
    });
}

#[test]
fn direct_and_indirect_eval_oom() {
    run_sweep(SweepCase {
        name: "direct_and_indirect_eval",
        setup: "",
        source: "(() => { function a() { let x = 1; return (() => { eval('var x = 2'); return x; })(); } function b(p = 3) { var p = 4; return eval('p'); } function c() { 'use strict'; return (function(p = 5) { var local = p + 1; return eval('local'); })(); } return `${a()},${b()},${c()},${(0, eval)('30 + 12')}`; })()",
        expected: "2,4,6,42",
        jobs: false,
        module: false,
        outcome: OomOutcome::Strict,
    });
}

#[test]
fn v8_stack_and_callsite_oom() {
    run_sweep(SweepCase {
        name: "v8_stack_and_callsite",
        setup: "",
        source: "(() => { Error.prepareStackTrace = (_, frames) => frames.map(f => `${f.getFunctionName()}:${f.isConstructor()}:${f.isNative()}:${f.getLineNumber()}`).join('|'); try { function Ctor() { return { s: [1].map(function inner() { return new Error('boom').stack; })[0] }; } const s = new Ctor().s; Error.prepareStackTrace = undefined; const d = {}; Error.captureStackTrace(d); return `${s.includes('Ctor:true:false')}:${typeof d.stack}`; } finally { Error.prepareStackTrace = undefined; } })()",
        expected: "true:string",
        jobs: false,
        module: false,
        outcome: OomOutcome::Strict,
    });
}

#[test]
fn generator_and_yield_star_oom() {
    run_sweep(SweepCase {
        name: "generator_and_yield_star",
        setup: "",
        source: "(() => { const g = (function*(){ yield 40; yield* [2]; })(); return g.next().value + g.next().value })()",
        expected: "42",
        jobs: false,
        module: false,
        outcome: OomOutcome::Strict,
    });
}

#[test]
fn async_function_and_jobs_oom() {
    run_sweep(SweepCase {
        name: "async_function_and_jobs",
        setup: "var result = 0; (async () => { return await Promise.resolve(42); })().then(x => { result = x }, e => { result = e.name + ':' + e.message });",
        source: "0",
        expected: "0",
        jobs: true,
        module: false,
        outcome: OomOutcome::OrAsyncCaught("InternalError:out of memory"),
    });
}

#[test]
fn async_generator_and_jobs_oom() {
    run_sweep(SweepCase {
        name: "async_generator_and_jobs",
        setup: "var result = 0; var g = (async function*(){ yield await 42; })(); g.next().then(v => { result = v.value }, e => { result = e.name + ':' + e.message });",
        source: "0",
        expected: "0",
        jobs: true,
        module: false,
        outcome: OomOutcome::OrAsyncCaught("InternalError:out of memory"),
    });
}

#[test]
fn promise_combinators_and_subclass_oom() {
    run_sweep(SweepCase {
        name: "promise_combinators_and_subclass",
        setup: "var result = 0; class Sub extends Promise {} Sub.all([Promise.resolve(40), 2]).then(x => { result = x[0] + x[1] }, e => { result = e.name + ':' + e.message });",
        source: "0",
        expected: "0",
        jobs: true,
        module: false,
        outcome: OomOutcome::OrAsyncCaught("InternalError:out of memory"),
    });
}

#[test]
fn weak_collections_oom() {
    run_sweep(SweepCase {
        name: "weak_collections",
        setup: "",
        source: "(() => { const key = {}; const value = {}; const m = new WeakMap([[key, value]]); const s = new WeakSet([key]); const w = new WeakRef(key); const r = new FinalizationRegistry(() => {}); r.register(key, value, key); return m.get(key) === value && s.has(key) && w.deref() === key && r.unregister(key); })()",
        expected: "true",
        jobs: false,
        module: false,
        outcome: OomOutcome::Strict,
    });
}

#[test]
fn finalization_registry_jobs_oom() {
    run_sweep(SweepCase {
        name: "finalization_registry_jobs",
        setup: "var cleanup = 0; var reg = new FinalizationRegistry(x => { cleanup += x }); (function(){ for (let i = 0; i < 9; i++) reg.register({}, 1); })();",
        source: "0",
        expected: "0",
        jobs: true,
        module: false,
        outcome: OomOutcome::Strict,
    });
}

#[test]
fn core_builtins_and_math_hypot_oom() {
    run_sweep(SweepCase {
        name: "core_builtins_and_math_hypot",
        setup: "",
        source: "(() => { const a = Array.from({ length: 24 }, (_, i) => i); const obj = Object.fromEntries(a.map(i => ['k_' + i, i])); const m = new Map(Object.entries(obj)); const s = new Set(m.values()); return `${s.union(new Set([99, 100])).size}:${Math.hypot(3, 4)}`; })()",
        expected: "26:5",
        jobs: false,
        module: false,
        outcome: OomOutcome::Strict,
    });
}

#[test]
fn json_and_unicode_oom() {
    run_sweep(SweepCase {
        name: "json_and_unicode",
        setup: "",
        source: "(() => { const s = 'e\\u0301'.repeat(20).normalize(); const v = { a: s, b: [1, 2, 'fox'], c: { extra: 'Ω' } }; return JSON.parse(JSON.stringify(v)).a.length })()",
        expected: "20",
        jobs: false,
        module: false,
        outcome: OomOutcome::Strict,
    });
}

#[test]
fn typed_array_and_dataview_oom() {
    run_sweep(SweepCase {
        name: "typed_array_and_dataview",
        setup: "",
        source: "(() => { const buf = new ArrayBuffer(8, { maxByteLength: 16 }); buf.resize(16); const a = new Uint16Array(buf); a.set([10, 2]); const dv = new DataView(buf); const first = dv.getUint16(0, true); const b = buf.transfer(); return `${first},${new Uint16Array(b).slice(0, 2).join(',')}` })()",
        expected: "10,10,2",
        jobs: false,
        module: false,
        outcome: OomOutcome::Strict,
    });
}

#[test]
fn proxy_traps_and_revoke_oom() {
    run_sweep(SweepCase {
        name: "proxy_traps_and_revoke",
        setup: "",
        source: "(() => { const p = Proxy.revocable({ x: 42 }, { ownKeys(t) { return Reflect.ownKeys(t); }, get(t, k) { return t[k]; } }); const x = Object.assign({}, p.proxy).x; p.revoke(); return x })()",
        expected: "42",
        jobs: false,
        module: false,
        outcome: OomOutcome::Strict,
    });
}

#[test]
fn bigint_arithmetic_oom() {
    run_sweep(SweepCase {
        name: "bigint_arithmetic",
        setup: "",
        source: "(() => { const a = BigInt('123456789123456789123456789'); return ((a ** 4n) / (a ** 3n)).toString().length })()",
        expected: "27",
        jobs: false,
        module: false,
        outcome: OomOutcome::Strict,
    });
}

#[test]
fn regexp_unicode_sets_and_backtrack_oom() {
    run_sweep(SweepCase {
        name: "regexp_unicode_sets_and_backtrack",
        setup: "",
        source: "(() => { const re = new RegExp('(?<a>[\\\\p{ASCII}&&\\\\p{Letter}]+)(a|aa)+c/-', 'dv'); return re.exec('aaaaaaac/-').indices.groups.a[0] })()",
        expected: "0",
        jobs: false,
        module: false,
        outcome: OomOutcome::Strict,
    });
}

#[test]
fn error_host_formatting_oom() {
    run_sweep(SweepCase {
        name: "error_host_formatting",
        setup: "",
        source: "(() => { throw { probe: 'probe' } })()",
        expected: "",
        jobs: false,
        module: false,
        outcome: OomOutcome::OrThrownFallback,
    });
}

#[test]
fn module_sync_import_attributes_and_retry_oom() {
    run_sweep(SweepCase {
        name: "module_sync_import_attributes_and_retry",
        setup: "var result = 0;",
        source: "import { n } from 'simple'; import j from 'data.json' with { type: 'json' }; import t from 'data.json' with { type: 'text' }; import plain from 'data.json'; globalThis.result = (j !== plain && JSON.parse(t).v === 41) ? n + 1 : -1; export { n };",
        expected: "",
        jobs: false,
        module: true,
        outcome: OomOutcome::Strict,
    });
}

#[test]
fn module_tla_and_retry_oom() {
    run_sweep(SweepCase {
        name: "module_tla_and_retry",
        setup: "var result = 0;",
        source: "import { n } from 'dep'; globalThis.result = n + 1; export { n };",
        expected: "",
        jobs: true,
        module: true,
        outcome: OomOutcome::Strict,
    });
}

#[test]
fn parser_and_compiler_memory_budget_oom() {
    // Measure bootstrap retained bytes, then construct an engine with tight
    // headroom so parser/compiler `max_bytes` checks trigger deterministically.
    let probe = JsEngine::new(EngineLimits::default()).unwrap();
    let base_used = probe.runtime().heap().ledger.used_bytes();
    drop(probe);

    let limits = EngineLimits {
        heap_bytes: base_used + 64 * 1024,
        ..EngineLimits::default()
    };
    let bare = bare_ledger_bytes(limits);
    let mut engine = JsEngine::new(limits).unwrap();
    let ledger = engine.runtime().heap().ledger.clone();
    let opts = EvalOptions::default();

    let big_script = format!("let x = [{}]; x.length", "1,".repeat(40_000));
    assert_eq!(
        engine.eval_to_string(&big_script, opts.clone()),
        Err(EngineError::OutOfMemory)
    );

    let big_module = format!(
        "export const arr = [{}];",
        "\"abcdef\",".repeat(15_000)
    );
    assert_eq!(
        engine.eval(
            &big_module,
            EvalOptions {
                module: true,
                filename: "big.mjs".into(),
                ..opts.clone()
            }
        ),
        Err(EngineError::OutOfMemory)
    );

    // Engine must still recover cleanly after tight-budget parse/compile OOM.
    engine.collect_garbage();
    assert_eq!(engine.eval_to_string("40 + 2", opts), Ok("42".into()));
    assert_clean_drop(engine, &ledger, bare, "parser_and_compiler_budget", 0);
}
