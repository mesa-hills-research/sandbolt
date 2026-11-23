#![deny(unsafe_code)]

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::mem::size_of;
use std::path::PathBuf;
use std::time::Instant;

use qjs_runtime::api::{EvalOptions, JsEngine, ModuleLoader, ResolvedModule};
use qjs_runtime::object::{JsObject, ObjectData};
use qjs_runtime::shape::Shape;
use qjs_runtime::string::JsString;
use qjs_runtime::EngineLimits;

const ARKO_PRELUDE: &str = include_str!("../tests/arko_code_mode.rs");

fn extract_prelude() -> &'static str {
    let marker = "const PRELUDE: &str = r#\"";
    let start = ARKO_PRELUDE.find(marker).expect("PRELUDE marker") + marker.len();
    let end = ARKO_PRELUDE[start..]
        .find("\"#;")
        .expect("PRELUDE end");
    &ARKO_PRELUDE[start..start + end]
}

struct MicrobenchLoader;

impl ModuleLoader for MicrobenchLoader {
    fn resolve(
        &mut self,
        _: &str,
        specifier: &str,
        _: Option<&str>,
    ) -> Result<ResolvedModule, String> {
        match specifier {
            "qjs:std" => Ok(ResolvedModule::Source {
                name: "qjs:std".into(),
                source: "export function open(_name, mode) { return mode === 'w' ? { puts() {}, close() {} } : null; } export function gc() {}".into(),
            }),
            "qjs:os" => Ok(ResolvedModule::Source {
                name: "qjs:os".into(),
                source: "export function now() { return Math.trunc(performance.now() * 1000); } export const cputime = undefined;".into(),
            }),
            other => Err(format!("unexpected module '{other}'")),
        }
    }
}

fn read_rss_kb() -> usize {
    let Ok(status) = fs::read_to_string("/proc/self/status") else {
        return 0;
    };
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            return rest
                .split_whitespace()
                .next()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
        }
    }
    0
}

fn print_memory_breakdown(label: &str, engine: &JsEngine) {
    let heap = engine.runtime().heap();
    let ledger_used = heap.ledger.used_bytes();
    let ledger_peak = heap.ledger.peak_bytes();
    println!("=== Memory Breakdown: {label} ===");
    println!(
        "  ledger used: {} B ({:.1} KB), peak: {} B ({:.1} KB), gc_threshold: {} B, collections: {}",
        ledger_used,
        ledger_used as f64 / 1024.0,
        ledger_peak,
        ledger_peak as f64 / 1024.0,
        heap.ledger.gc_threshold(),
        heap.collection_count()
    );
    println!(
        "  sizes: JsObject={} B, ObjectData={} B, Shape={} B, JsString={} B",
        size_of::<JsObject>(),
        size_of::<ObjectData>(),
        size_of::<Shape>(),
        size_of::<JsString>(),
    );
    macro_rules! arena_line {
        ($name:expr, $arena:expr, $ty:ty) => {{
            let live = $arena.len();
            let cap = $arena.capacity();
            // Slot<T> adds generation(2)+next_free(8)+mark(1)+Option tag (~16 B align)
            let est_slot = size_of::<Option<$ty>>() + 16;
            println!(
                "  arena {:<12}: live={:<6} cap={:<6} elem={}B est_cap_bytes={}B",
                $name,
                live,
                cap,
                size_of::<$ty>(),
                cap * est_slot
            );
        }};
    }
    arena_line!("objects", heap.objects, JsObject);
    arena_line!("shapes", heap.shapes, Shape);
    arena_line!("strings", heap.strings, JsString);
    arena_line!("symbols", heap.symbols, qjs_runtime::symbol::SymbolData);
    arena_line!("bigints", heap.bigints, qjs_runtime::heap::BigIntData);
    arena_line!("bytecode", heap.bytecode, qjs_runtime::vm::BytecodeFunc);
    arena_line!("modules", heap.modules, qjs_runtime::vm::ModuleRecord);
    arena_line!("var_refs", heap.var_refs, qjs_runtime::vm::VarRef);
    arena_line!("accessors", heap.accessors, qjs_runtime::object::Accessor);
    arena_line!("coroutines", heap.coroutines, qjs_runtime::vm::CoroutineState);
    arena_line!("realms", heap.realms, qjs_runtime::realm::Realm);

    let mut variant_counts: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut obj_props_cap_bytes = 0usize;
    let mut obj_props_len_total = 0usize;
    let mut obj_props_nonempty = 0usize;
    for (_, obj) in heap.objects.iter() {
        let tag = match &obj.data {
            ObjectData::Ordinary => "Ordinary",
            ObjectData::NativeFunction(_) => "NativeFunction",
            ObjectData::BytecodeFunction(_) => "BytecodeFunction",
            ObjectData::Array(_) => "Array",
            ObjectData::Error(_) => "Error",
            ObjectData::BoundFunction(_) => "BoundFunction",
            ObjectData::RegExp(_) => "RegExp",
            ObjectData::Map(_) => "Map",
            ObjectData::Set(_) => "Set",
            ObjectData::Promise(_) => "Promise",
            _ => "Other",
        };
        *variant_counts.entry(tag).or_default() += 1;
        obj_props_cap_bytes += obj.props.capacity() * size_of::<qjs_runtime::value::Value>();
        obj_props_len_total += obj.props.len();
        if obj.props.capacity() > 0 {
            obj_props_nonempty += 1;
        }
    }
    println!(
        "  objects detail: nonempty_props={obj_props_nonempty}, total_prop_len={obj_props_len_total}, props_vec_bytes={obj_props_cap_bytes} B"
    );
    println!("  object variants: {variant_counts:?}");

    let mut shape_props_bytes = 0usize;
    let mut shape_props_total = 0usize;
    let mut shape_small = 0usize;
    let mut shape_empty = 0usize;
    let mut shape_dict = 0usize;
    for (_, shape) in heap.shapes.iter() {
        shape_props_bytes += shape.props.capacity() * size_of::<qjs_runtime::shape::ShapeProp>();
        shape_props_total += shape.props.len();
        if shape.props.is_empty() {
            shape_empty += 1;
        } else if shape.props.len() <= 4 {
            shape_small += 1;
        }
        if shape.dictionary {
            shape_dict += 1;
        }
    }
    println!(
        "  shapes detail: total={}, empty={shape_empty}, small(1..=4)={shape_small}, dict={shape_dict}, total_props={shape_props_total}, props_vec_bytes={shape_props_bytes} B",
        heap.shapes.len()
    );

    let mut str_bytes = 0usize;
    let mut str_le1 = 0usize;
    let mut str_le16 = 0usize;
    let mut str_le30 = 0usize;
    let mut str_gt30 = 0usize;
    let mut str_wide = 0usize;
    for (_, s) in heap.strings.iter() {
        let len = s.len() as usize;
        match s {
            JsString::Latin1 { data, .. } => str_bytes += data.capacity(),
            JsString::Utf16(data) => {
                str_bytes += data.capacity() * 2;
                str_wide += 1;
            }
            JsString::InlineLatin1 { .. } | JsString::Rope { .. } => {}
        }
        if len <= 1 {
            str_le1 += 1;
        } else if len <= 16 {
            str_le16 += 1;
        } else if len <= 30 {
            str_le30 += 1;
        } else {
            str_gt30 += 1;
        }
    }
    println!(
        "  strings detail: total={}, buf_bytes={str_bytes} B, wide={str_wide}, len<=1:{str_le1}, 2..=16:{str_le16}, 17..=30:{str_le30}, >30:{str_gt30}",
        heap.strings.len()
    );
}

fn bench_startup_and_memory() {
    let mut fresh = JsEngine::new(EngineLimits::default()).unwrap();
    print_memory_breakdown("JsEngine::new (before GC)", &fresh);
    fresh.collect_garbage();
    print_memory_breakdown("JsEngine::new (after GC)", &fresh);

    let prelude = extract_prelude();
    fresh
        .eval(
            prelude,
            EvalOptions {
                strict: true,
                filename: "prelude.js".into(),
                ..Default::default()
            },
        )
        .unwrap();
    print_memory_breakdown("JsEngine::new + Arko prelude (before GC)", &fresh);
    fresh.collect_garbage();
    print_memory_breakdown("JsEngine::new + Arko prelude (after GC)", &fresh);
    drop(fresh);

    // Measure JsEngine::new latency (median of 50)
    let mut new_times = Vec::with_capacity(50);
    for _ in 0..50 {
        let t0 = Instant::now();
        let e = JsEngine::new(EngineLimits::default()).unwrap();
        let dt = t0.elapsed().as_nanos();
        drop(e);
        new_times.push(dt);
    }
    new_times.sort_unstable();
    println!(
        "Startup JsEngine::new (n=50): p50={:.3} ms, min={:.3} ms, p95={:.3} ms",
        new_times[25] as f64 / 1e6,
        new_times[0] as f64 / 1e6,
        new_times[47] as f64 / 1e6,
    );

    // Measure JsEngine::new + prelude latency (median of 30)
    let mut prelude_times = Vec::with_capacity(30);
    for _ in 0..30 {
        let t0 = Instant::now();
        let mut e = JsEngine::new(EngineLimits::default()).unwrap();
        e.eval(
            prelude,
            EvalOptions {
                strict: true,
                filename: "prelude.js".into(),
                ..Default::default()
            },
        )
        .unwrap();
        let dt = t0.elapsed().as_nanos();
        drop(e);
        prelude_times.push(dt);
    }
    prelude_times.sort_unstable();
    println!(
        "Startup JsEngine::new + Arko prelude (n=30): p50={:.3} ms, min={:.3} ms, p95={:.3} ms",
        prelude_times[15] as f64 / 1e6,
        prelude_times[0] as f64 / 1e6,
        prelude_times[28] as f64 / 1e6,
    );

    // Measure RSS delta across 32 retained engines
    let rss_before = read_rss_kb();
    let mut engines = Vec::with_capacity(32);
    for _ in 0..32 {
        let mut e = JsEngine::new(EngineLimits::default()).unwrap();
        e.collect_garbage();
        engines.push(e);
    }
    let rss_after = read_rss_kb();
    let per_engine_kb = rss_after.saturating_sub(rss_before) as f64 / 32.0;
    println!(
        "RSS across 32 retained post-GC engines: before={rss_before} KB, after={rss_after} KB, delta={:.1} KB/engine",
        per_engine_kb
    );
    drop(engines);
}

fn bench_gc_and_uri(test262_root: Option<&str>) {
    // 1. Transient spike + GC retention
    let mut e = JsEngine::new(EngineLimits::default()).unwrap();
    e.collect_garbage();
    let base_used = e.runtime().heap().ledger.used_bytes();
    let t0 = Instant::now();
    e.eval(
        "(() => { let arr = []; for (let i = 0; i < 100000; i++) arr.push({ a: i, b: 'k' + (i & 63) }); return arr.length; })()",
        EvalOptions::default(),
    )
    .unwrap();
    let spike_peak = e.runtime().heap().ledger.peak_bytes();
    let spike_before_gc = e.runtime().heap().ledger.used_bytes();
    e.collect_garbage();
    let spike_after_gc = e.runtime().heap().ledger.used_bytes();
    let dt = t0.elapsed().as_secs_f64() * 1000.0;
    println!(
        "Transient 100k-object spike: time={dt:.1} ms, base={base_used} B, peak={spike_peak} B, before_gc={spike_before_gc} B, after_gc={spike_after_gc} B, objects(live/cap)={}/{}, strings(live/cap)={}/{}",
        e.runtime().heap().objects.len(),
        e.runtime().heap().objects.capacity(),
        e.runtime().heap().strings.len(),
        e.runtime().heap().strings.capacity(),
    );
    drop(e);

    // 2. Exhaustive decodeURI S15.1.3.1_A2.5_T1.js if test262_root is provided
    if let Some(root) = test262_root {
        let path = PathBuf::from(root)
            .join("test/built-ins/decodeURI/S15.1.3.1_A2.5_T1.js");
        let sta = PathBuf::from(root).join("harness/sta.js");
        let assert_js = PathBuf::from(root).join("harness/assert.js");
        let hex_js = PathBuf::from(root).join("harness/decimalToHexString.js");
        if path.exists() {
            let mut e = JsEngine::new(EngineLimits::default()).unwrap();
            for h in [sta, assert_js, hex_js] {
                let src = fs::read_to_string(&h).unwrap();
                e.eval(&src, EvalOptions::default()).unwrap();
            }
            let src = fs::read_to_string(&path).unwrap();
            let t0 = Instant::now();
            e.eval(&src, EvalOptions::default()).unwrap();
            let dt = t0.elapsed().as_secs_f64() * 1000.0;
            let peak = e.runtime().heap().ledger.peak_bytes();
            let used = e.runtime().heap().ledger.used_bytes();
            let gcs = e.runtime().heap().collection_count();
            println!(
                "decodeURI/S15.1.3.1_A2.5_T1.js: time={dt:.1} ms, peak={:.2} MB, final_used={:.2} MB, gcs={gcs}",
                peak as f64 / (1024.0 * 1024.0),
                used as f64 / (1024.0 * 1024.0),
            );
        }
    }
}

fn bench_microbench(path: &str, filter: &[String]) {
    let source = fs::read_to_string(path).expect("read microbench.js");
    let mut engine = JsEngine::new(EngineLimits::default()).unwrap();
    engine.set_module_loader(Box::new(MicrobenchLoader));
    engine.set_print_callback(Box::new(|line| {
        use std::io::Write;
        println!("{line}");
        let _ = std::io::stdout().flush();
    }));
    let args_json = std::iter::once("microbench.js")
        .chain(filter.iter().map(String::as_str))
        .map(|s| format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"")))
        .collect::<Vec<_>>()
        .join(", ");
    engine
        .eval(
            &format!(
                "globalThis.console = {{ log(...a) {{ print(a.join(' ')); }} }}; globalThis.scriptArgs = [{args_json}];"
            ),
            EvalOptions::default(),
        )
        .unwrap();
    engine
        .eval(
            &source,
            EvalOptions {
                module: true,
                filename: "microbench.js".into(),
                ..Default::default()
            },
        )
        .unwrap();
    engine.run_jobs().unwrap();
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("memory") => bench_startup_and_memory(),
        Some("gc") => bench_gc_and_uri(args.get(1).map(String::as_str)),
        Some("microbench") => {
            let path = args
                .get(1)
                .map(String::as_str)
                .unwrap_or("/box/data/quickjs-ref/tests/microbench.js");
            bench_microbench(path, &args[2..]);
        }
        _ => {
            eprintln!("usage: bench <memory | gc [test262_root] | microbench [path] [filters...]>");
            std::process::exit(2);
        }
    }
}
