use std::cmp::Ordering;
use std::collections::HashMap;
use std::fs;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use std::sync::mpsc;
use std::thread;

use qjs_runtime::api::{EvalOptions, JsEngine};
use qjs_runtime::{EngineError, EngineLimits};
use qjs_syntax::{ParseGoal, ParseOptions, SourceText};

use crate::config::{Config, Mode, Options, read};
use crate::frontmatter::{Metadata, Phase};
use crate::harness::{self, Budget, Session};
use crate::loader::{Fixtures, is_test};
use crate::report::{Exception, Execution, Key, Outcome};

pub(crate) struct Results {
    pub executions: Vec<Execution>,
    pub skipped: usize,
    pub excluded: usize,
    pub notices: Vec<String>,
}

pub(crate) fn run(config: &Config, options: &Options) -> Result<Results, String> {
    let (files, excluded) = discover(config, options.filter.as_deref(), options.limit)?;
    let mut results = Results {
        executions: Vec::new(),
        skipped: 0,
        excluded,
        notices: Vec::new(),
    };
    let mode = if options.all_modes {
        Mode::Both
    } else {
        config.mode
    };
    let completed = in_parallel(&files, options.threads, HashMap::new, |cache, path| {
        let name = config.name(path);
        let result = isolate(|| run_file(config, options, mode, cache, path, &name));
        match result {
            Ok(file) => file,
            Err(message) => FileResult {
                executions: vec![host_failure(name, false, message)],
                ..FileResult::default()
            },
        }
    });
    for file in completed {
        results.notices.extend(file.notices);
        results.skipped += usize::from(file.skipped);
        results.executions.extend(file.executions);
    }
    Ok(results)
}

fn in_parallel<T: Sync, C, R: Send>(
    tasks: &[T],
    workers: usize,
    init: impl Fn() -> C + Sync,
    work: impl Fn(&mut C, &T) -> R + Sync,
) -> Vec<R> {
    let next = AtomicUsize::new(0);
    let (send, receive) = mpsc::channel();
    thread::scope(|scope| {
        for _ in 0..workers.min(tasks.len()) {
            let (next, init, work, send) = (&next, &init, &work, send.clone());
            scope.spawn(move || {
                let mut context = init();
                loop {
                    let index = next.fetch_add(1, AtomicOrdering::Relaxed);
                    let Some(task) = tasks.get(index) else { break };
                    let result = work(&mut context, task);
                    if send.send((index, result)).is_err() {
                        break;
                    }
                }
            });
        }
        drop(send);
        let mut replies: Vec<_> = receive.into_iter().collect();
        replies.sort_unstable_by_key(|(index, _)| *index);
        replies.into_iter().map(|(_, reply)| reply).collect()
    })
}

fn discover(
    config: &Config,
    filter: Option<&str>,
    limit: Option<usize>,
) -> Result<(Vec<PathBuf>, usize), String> {
    let mut files: Vec<_> = config
        .tests
        .iter()
        .filter(|path| is_test(path))
        .cloned()
        .collect();
    for directory in &config.directories {
        scan(directory, &mut files)?;
    }
    let mut files: Vec<_> = files
        .into_iter()
        .map(|path| (config.name(&path), path))
        .collect();
    files.sort_by(|a, b| natural_cmp(&a.0, &b.0));
    files.dedup_by(|a, b| natural_cmp(&a.0, &b.0) == Ordering::Equal);
    let mut excluded = 0;
    files.retain(|(name, path)| {
        if filter.is_some_and(|filter| !name.contains(filter)) {
            return false;
        }
        if config.excluded(path) {
            excluded += 1;
            false
        } else {
            true
        }
    });
    if let Some(limit) = limit {
        files.truncate(limit);
    }
    Ok((files.into_iter().map(|(_, path)| path).collect(), excluded))
}

fn scan(path: &Path, files: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries = fs::read_dir(path).map_err(|error| format!("{}: {error}", path.display()))?;
    for entry in entries {
        let entry = entry.map_err(|error| format!("{}: {error}", path.display()))?;
        let path = entry.path();
        let kind = entry
            .file_type()
            .map_err(|error| format!("{}: {error}", path.display()))?;
        if kind.is_dir() {
            scan(&path, files)?;
        } else if is_test(&path) {
            files.push(path);
        }
    }
    Ok(())
}

pub(crate) fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (mut a, mut b) = (a.as_bytes(), b.as_bytes());
    while let (Some(&ac), Some(&bc)) = (a.first(), b.first()) {
        if ac.is_ascii_digit() && bc.is_ascii_digit() {
            let a_end = a.iter().take_while(|c| c.is_ascii_digit()).count();
            let b_end = b.iter().take_while(|c| c.is_ascii_digit()).count();
            let ax = trim_zero(&a[..a_end]);
            let bx = trim_zero(&b[..b_end]);
            let result = ax.len().cmp(&bx.len()).then(ax.cmp(bx));
            if result != Ordering::Equal {
                return result;
            }
            (a, b) = (&a[a_end..], &b[b_end..]);
        } else if ac == bc {
            (a, b) = (&a[1..], &b[1..]);
        } else {
            return ac.cmp(&bc);
        }
    }
    a.len().cmp(&b.len())
}

fn trim_zero(digits: &[u8]) -> &[u8] {
    let offset = digits.iter().take_while(|&&ch| ch == b'0').count();
    &digits[offset..]
}

type Cache = HashMap<PathBuf, Result<String, String>>;

#[derive(Default)]
struct FileResult {
    executions: Vec<Execution>,
    notices: Vec<String>,
    skipped: bool,
}

fn run_file(
    config: &Config,
    options: &Options,
    mode: Mode,
    cache: &mut Cache,
    path: &Path,
    name: &str,
) -> FileResult {
    let (source, metadata) =
        match read(path).and_then(|source| Metadata::parse(&source).map(|meta| (source, meta))) {
            Ok(test) => test,
            Err(error) => {
                return FileResult {
                    executions: vec![host_failure(name.into(), false, error)],
                    ..FileResult::default()
                };
            }
        };
    let mut notices = Vec::new();
    let Some(plan) = plan(config, mode, &metadata, name, &mut notices) else {
        return FileResult {
            notices,
            skipped: true,
            ..FileResult::default()
        };
    };
    let module = if config.local && !metadata.flag("qjs:no-detect-module") {
        detect_module(name, &source)
    } else {
        metadata.flag("module")
    };
    let harness_dir = config.harness.clone().or_else(|| {
        path.ancestors()
            .find(|dir| dir.file_name().is_some_and(|name| name == "test"))
            .and_then(Path::parent)
            .map(|base| base.join("harness"))
    });
    let test = Test {
        config,
        options,
        path,
        name,
        source: &source,
        metadata: &metadata,
        harness_dir,
        module,
        includes: &plan.includes,
    };
    let mut executions = Vec::with_capacity(plan.variants.len());
    for &strict in plan.variants {
        let result = isolate(|| execute(&test, strict, cache));
        executions.push(match result {
            Ok(result) => result,
            Err(message) => host_failure(name.into(), strict, message),
        });
    }
    FileResult {
        executions,
        notices,
        skipped: false,
    }
}

fn detect_module(name: &str, source: &str) -> bool {
    let text = SourceText::from_utf8(name, source);
    qjs_syntax::parse(
        &text,
        ParseOptions {
            goal: ParseGoal::Module,
            ..Default::default()
        },
    )
    .is_ok()
}

struct Plan<'a> {
    variants: &'static [bool],
    includes: Vec<&'a str>,
}

fn plan<'a>(
    config: &Config,
    mode: Mode,
    metadata: &'a Metadata,
    name: &str,
    notices: &mut Vec<String>,
) -> Option<Plan<'a>> {
    let mut skip = (config.skip_async && metadata.flag("async"))
        || (config.skip_module && metadata.flag("module"));
    for feature in &metadata.features {
        if config.features.contains(feature) {
            continue;
        }
        skip = true;
        if !config.skip_features.contains(feature) {
            notices.push(format!("{name}:1: unknown feature: {feature}"));
        }
    }
    skip |= metadata
        .includes
        .iter()
        .any(|include| config.harness_excludes.contains(include));
    let variants = mode.variants(metadata);
    if skip || variants.is_empty() {
        return None;
    }
    let mut includes = Vec::new();
    if !config.local {
        includes.extend(["sta.js", "assert.js"]);
    }
    includes.extend(metadata.includes.iter().map(String::as_str));
    if metadata.flag("async") {
        includes.push("doneprintHandle.js");
    }
    Some(Plan { variants, includes })
}

struct Test<'a> {
    config: &'a Config,
    options: &'a Options,
    path: &'a Path,
    name: &'a str,
    source: &'a str,
    metadata: &'a Metadata,
    harness_dir: Option<PathBuf>,
    module: bool,
    includes: &'a [&'a str],
}

fn execute(test: &Test<'_>, strict: bool, cache: &mut Cache) -> Execution {
    let budget = Budget::new(test.options.timeout);
    let mut result = Execution {
        key: Key {
            path: test.name.into(),
            strict,
        },
        negative: test.metadata.negative.clone(),
        outcome: Outcome::Complete,
        heap_bytes: None,
        output: Vec::new(),
        test_line: first_code_line(test.source),
    };
    let mut engine = match JsEngine::new(EngineLimits::default()) {
        Ok(engine) => engine,
        Err(error) => {
            return host_failure(test.name.into(), strict, format!("engine setup: {error:?}"));
        }
    };
    let session = Session::new(budget.clone(), test.config.verbose > 1);
    let countdown = Rc::new(harness::Countdown::default());
    harness::interrupt(&mut engine, budget.clone(), countdown.clone());
    engine.set_can_block(!test.metadata.flag("CanBlockIsFalse"));
    engine.set_module_loader(Box::new(Fixtures::new(
        &test.config.root,
        test.path,
        test.config.local,
    )));
    result.outcome = match session.install(&mut engine) {
        Ok(()) => evaluate(
            &mut engine,
            &session,
            test,
            strict,
            cache,
            &countdown,
            result.test_line,
        ),
        Err(error) => Outcome::HostFailure(format!("host setup: {error:?}")),
    };
    if test.options.memory > 0 {
        result.heap_bytes = Some(engine.heap_bytes());
    }
    session.finish();
    result.output.extend(session.output());
    if budget.expired() {
        result.outcome = Outcome::HostFailure(format!(
            "timed out after {} ms",
            test.options.timeout.as_millis()
        ));
    }
    if countdown.fired.get() && !budget.expired() {
        result.outcome = Outcome::Exception(Exception {
            phase: Phase::Runtime,
            name: Some("InternalError".into()),
            message: "InternalError: interrupted".into(),
            line: result.test_line,
        });
    }
    result
}

fn evaluate(
    engine: &mut JsEngine,
    session: &Session,
    test: &Test<'_>,
    strict: bool,
    cache: &mut Cache,
    countdown: &harness::Countdown,
    test_line: u32,
) -> Outcome {
    for include in test.includes {
        let Some(dir) = &test.harness_dir else {
            return Outcome::HostFailure(format!("no harness directory for {include}"));
        };
        let path = dir.join(include);
        let source = match cache.entry(path.clone()).or_insert_with(|| read(&path)) {
            Ok(source) => source,
            Err(error) => return Outcome::HostFailure(error.clone()),
        };
        if let Err(error) = engine.eval(
            source,
            EvalOptions {
                filename: test.config.name(&path),
                ..Default::default()
            },
        ) {
            return Outcome::HostFailure(format!("harness {include}: {}", engine_message(&error)));
        }
    }
    session.reset_async();
    countdown
        .enabled
        .set(test.metadata.flag("qjs:set-interrupt-handler"));
    let options = EvalOptions {
        filename: test.name.into(),
        strict,
        module: test.module,
        annex_b: true,
    };
    match eval(engine, test.source, options, test_line) {
        Err(error) => error,
        Ok(()) => {
            let is_async = test.metadata.flag("async");
            if is_async || test.module || test.config.local {
                if let Err(error) = jobs(engine, test.name, test_line) {
                    return error;
                }
                if is_async && let Err(message) = session.async_result() {
                    if let Some(message) = message {
                        session.note(message);
                    }
                    return Outcome::Exception(Exception {
                        phase: Phase::Runtime,
                        name: Some("TypeError".into()),
                        message: "TypeError: $DONE() not called".into(),
                        line: test_line,
                    });
                }
            }
            Outcome::Complete
        }
    }
}

#[cfg(feature = "integrated-runtime")]
fn eval(
    engine: &mut JsEngine,
    source: &str,
    options: EvalOptions,
    line: u32,
) -> Result<(), Outcome> {
    let name = options.filename.clone();
    engine
        .eval_detailed(source, options)
        .map_err(|error| observation(error, &name, line))
}

#[cfg(not(feature = "integrated-runtime"))]
fn eval(engine: &mut JsEngine, source: &str, options: EvalOptions, _: u32) -> Result<(), Outcome> {
    engine
        .eval(source, options)
        .map_err(|error| Outcome::HostFailure(engine_message(&error)))
}

#[cfg(feature = "integrated-runtime")]
fn jobs(engine: &mut JsEngine, name: &str, line: u32) -> Result<(), Outcome> {
    engine
        .run_jobs_detailed()
        .map_err(|error| observation(error, name, line))
}

#[cfg(not(feature = "integrated-runtime"))]
fn jobs(engine: &mut JsEngine, _: &str, _: u32) -> Result<(), Outcome> {
    engine
        .run_jobs()
        .map_err(|error| Outcome::HostFailure(engine_message(&error)))
}

#[cfg(feature = "integrated-runtime")]
fn observation(failure: qjs_runtime::api::EvalFailure, name: &str, line: u32) -> Outcome {
    use qjs_runtime::api::EvalPhase;
    let error_name = failure
        .error_kind
        .map(|kind| kind.name().to_owned())
        .or(failure.error_name);
    let phase = match failure.phase {
        EvalPhase::Parse => Phase::Parse,
        EvalPhase::Resolution => Phase::Resolution,
        EvalPhase::Runtime => Phase::Runtime,
    };
    match failure.error {
        EngineError::Thrown { message, frames } => Outcome::Exception(Exception {
            phase,
            name: error_name,
            message,
            line: frames
                .iter()
                .find(|frame| frame.file == name)
                .map_or(line, |frame| frame.line),
        }),
        error => Outcome::HostFailure(engine_message(&error)),
    }
}

fn engine_message(error: &EngineError) -> String {
    match error {
        EngineError::Thrown { message, .. } | EngineError::Failed(message) => message.clone(),
        EngineError::Interrupted => "execution interrupted".into(),
        EngineError::OutOfMemory => "engine ran out of memory".into(),
    }
}

fn host_failure(path: String, strict: bool, message: String) -> Execution {
    Execution {
        key: Key { path, strict },
        negative: None,
        outcome: Outcome::HostFailure(message),
        heap_bytes: None,
        output: Vec::new(),
        test_line: 1,
    }
}

fn isolate<T>(run: impl FnOnce() -> T) -> Result<T, String> {
    catch_unwind(AssertUnwindSafe(run)).map_err(|panic| panic_message(&*panic))
}

fn panic_message(panic: &(dyn std::any::Any + Send)) -> String {
    let message = panic
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| panic.downcast_ref::<&str>().copied());
    message.map_or_else(
        || "runner panicked".into(),
        |message| format!("runner panicked: {message}"),
    )
}

fn first_code_line(source: &str) -> u32 {
    let mut rest = source;
    loop {
        rest = rest.trim_start();
        if let Some(comment) = rest.strip_prefix("//") {
            rest = comment.split_once('\n').map_or("", |(_, rest)| rest);
        } else if let Some(comment) = rest.strip_prefix("/*") {
            rest = comment.split_once("*/").map_or("", |(_, rest)| rest);
        } else {
            break;
        }
    }
    let consumed = if rest.is_empty() {
        source
    } else {
        &source[..source.len() - rest.len()]
    };
    1 + consumed.bytes().filter(|&ch| ch == b'\n').count() as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::Scratch;
    use std::collections::BTreeSet;

    #[test]
    fn selection_sorts_and_excludes_fixtures() {
        let tree = Scratch::new();
        tree.write("cases/2.js", "");
        tree.write("cases/10.js", "");
        tree.write("cases/00.js", "");
        tree.write("cases/fixture_FIXTURE.js", "");
        tree.write("cases/skip.js", "");
        tree.write("cases/dir/one.js", "");
        tree.write("cases/dir/keep/one.js", "");
        let path = tree.write("test262.conf", "[config]\ntestdir=cases\n[tests]\ncases/2.js\n[exclude]\ncases/skip.js\ncases/dir/\n!cases/dir/keep/\n");
        let config = Config::load(&path).unwrap();
        let (files, excluded) = discover(&config, None, Some(3)).unwrap();
        assert_eq!(
            files
                .iter()
                .map(|path| config.name(path))
                .collect::<Vec<_>>(),
            ["cases/00.js", "cases/2.js", "cases/10.js"]
        );
        assert_eq!(excluded, 2);
        let (files, _) = discover(&config, Some("dir/"), None).unwrap();
        assert_eq!(
            files
                .iter()
                .map(|path| config.name(path))
                .collect::<Vec<_>>(),
            ["cases/dir/keep/one.js"]
        );
        assert_eq!(natural_cmp("p/a001e", "p/a1e"), Ordering::Equal);
    }

    #[test]
    fn plans_variants_and_includes() {
        let mut config = Config::default();
        config.features.insert("ok".into());
        config.skip_features.insert("off".into());
        let mut notices = Vec::new();
        let meta = Metadata {
            flags: ["async", "module"].map(String::from).into(),
            features: vec!["ok".into()],
            includes: vec!["extra.js".into()],
            ..Default::default()
        };
        let plan = plan(&config, Mode::Both, &meta, "x.js", &mut notices).unwrap();
        assert_eq!(plan.variants, &[false]);
        assert_eq!(
            plan.includes,
            ["sta.js", "assert.js", "extra.js", "doneprintHandle.js"]
        );
        assert!(notices.is_empty());
        let metadata = |flag: &str| Metadata {
            flags: vec![flag.into()],
            ..Default::default()
        };
        assert_eq!(Mode::Both.variants(&metadata("onlyStrict")), &[true]);
        assert_eq!(Mode::Both.variants(&metadata("raw")), &[false]);
        assert_eq!(Mode::Both.variants(&metadata("noStrict")), &[false]);
        assert_eq!(Mode::Both.variants(&Metadata::default()), &[false, true]);
        assert_eq!(Mode::Strict.variants(&metadata("noStrict")), &[]);
        assert_eq!(Mode::Sloppy.variants(&metadata("onlyStrict")), &[]);
        assert_eq!(Mode::DefaultStrict.variants(&metadata("raw")), &[false]);
        assert_eq!(
            Mode::DefaultSloppy.variants(&metadata("onlyStrict")),
            &[true]
        );
        assert!(
            super::plan(
                &config,
                Mode::Both,
                &Metadata {
                    features: vec!["missing".into(), "off".into()],
                    ..Default::default()
                },
                "x.js",
                &mut notices
            )
            .is_none()
        );
        assert_eq!(notices, ["x.js:1: unknown feature: missing"]);
    }

    #[test]
    fn local_module_detection_matches_compile_only_probe() {
        assert!(detect_module("test.js", "let x = 1;"));
        assert!(detect_module("test.js", "import x from './fixture.js';"));
        assert!(!detect_module("test.js", "var await = 42;"));
        assert!(!detect_module("test.js", "/* comment */ with ({}) {}"));
    }

    #[test]
    fn workers_return_results_in_selection_order() {
        let slots: Vec<_> = (0..512).collect();
        let results = in_parallel(
            &slots,
            8,
            || None::<thread::ThreadId>,
            |worker, task| {
                let id = thread::current().id();
                assert_eq!(*worker.get_or_insert(id), id);
                *task * 2
            },
        );
        assert_eq!(results, (0..512).map(|value| value * 2).collect::<Vec<_>>());
    }

    #[test]
    fn a_panicking_test_does_not_stop_the_pool() {
        let results = in_parallel(
            &[0, 1, 2, 3],
            2,
            || (),
            |_, task| {
                isolate(|| {
                    assert_ne!(*task, 1, "test panic");
                    *task
                })
            },
        );
        assert_eq!(results.len(), 4);
        assert!(results[1].as_ref().unwrap_err().contains("test panic"));
        assert_eq!(results[3], Ok(3));
    }

    #[cfg(feature = "integrated-runtime")]
    #[test]
    fn observes_the_three_negative_phases() {
        let tree = Scratch::new();
        let path = tree.write("tests.conf", "[config]\nlocal=yes\ntestdir=cases\n");
        tree.write("cases/dep_FIXTURE.js", "export const present = 1;");
        tree.write("cases/parse.js", "/*---\nflags: [qjs:no-detect-module]\nnegative: {phase: parse, type: SyntaxError}\n---*/\nconst = 1;");
        tree.write("cases/resolution.js", "/*---\nflags: [module]\nnegative: {phase: resolution, type: SyntaxError}\n---*/\nimport {missing} from './dep_FIXTURE.js';");
        tree.write("cases/runtime.js", "/*---\nflags: [qjs:no-detect-module]\nnegative: {phase: runtime, type: TypeError}\n---*/\nnull.x;");
        tree.write("cases/wrong-phase.js", "/*---\nflags: [qjs:no-detect-module]\nnegative: {phase: parse, type: TypeError}\n---*/\nnull.x;");
        tree.write("cases/wrong-type.js", "/*---\nflags: [qjs:no-detect-module]\nnegative: {phase: runtime, type: TypeError}\n---*/\nthrow 'TypeError: text';");
        let config = Config::load(&path).unwrap();
        let options = Options::parse(["-j".into(), "3".into()]).unwrap().unwrap();
        let results = run(&config, &options).unwrap();
        assert_eq!(results.executions.len(), 5);
        for run in results.executions {
            assert!(
                run.failure(false).is_none(),
                "{}: {:?}",
                run.key.path,
                run.outcome
            );
            assert_eq!(
                run.failure(true).is_some(),
                run.key.path.contains("wrong-"),
                "{}: {:?}",
                run.key.path,
                run.outcome
            );
        }
    }

    #[cfg(feature = "integrated-runtime")]
    #[test]
    fn async_jobs_finish_and_a_loop_times_out() {
        let tree = Scratch::new();
        let path = tree.write(
            "tests.conf",
            "[config]\nlocal=yes\ntestdir=cases\nharnessdir=harness\n",
        );
        tree.write(
            "harness/doneprintHandle.js",
            "function $DONE() { print('Test262:AsyncTestComplete'); }",
        );
        tree.write("cases/async.js", "/*---\nflags: [async, qjs:no-detect-module]\n---*/\nPromise.resolve().then(() => $DONE());");
        tree.write(
            "cases/loop.js",
            "/*---\nflags: [qjs:no-detect-module]\n---*/\nwhile (true) {}",
        );
        let config = Config::load(&path).unwrap();
        let options = Options::parse(["--timeout-ms".into(), "150".into()])
            .unwrap()
            .unwrap();
        let results = run(&config, &options).unwrap();
        assert_eq!(results.executions.len(), 2);
        for run in results.executions {
            if run.key.path.ends_with("async.js") {
                assert!(run.failure(true).is_none(), "{:?}", run.outcome);
            } else {
                assert!(
                    matches!(run.outcome, Outcome::HostFailure(message) if message == "timed out after 150 ms")
                );
            }
        }
    }

    #[test]
    #[ignore = "set QUICKJS_REF to the reference checkout"]
    fn pinned_execution_selection() {
        let root = PathBuf::from(std::env::var_os("QUICKJS_REF").unwrap());
        for (config_name, mode, expected) in [
            ("test262.conf", Mode::Both, (48770, 4802, 5736, 82220)),
            ("tests.conf", Mode::DefaultSloppy, (116, 9, 0, 116)),
        ] {
            let config = Config::load(&root.join(config_name)).unwrap();
            let (files, excluded) = discover(&config, None, None).unwrap();
            let mut skipped = 0;
            let mut executions = 0;
            let mut unknown = BTreeSet::new();
            for file in &files {
                let source = read(file).unwrap();
                let metadata = Metadata::parse(&source)
                    .unwrap_or_else(|error| panic!("{}: {error}", file.display()));
                let mut warnings = Vec::new();
                match plan(&config, mode, &metadata, &config.name(file), &mut warnings) {
                    Some(plan) => executions += plan.variants.len(),
                    None => skipped += 1,
                }
                unknown.extend(warnings);
            }
            assert_eq!(
                (files.len(), excluded, skipped, executions),
                expected,
                "{unknown:?}"
            );
        }
    }
}
