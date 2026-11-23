//! Run Test262 and QuickJS's local tests in independent runtimes.

#![deny(unsafe_code)]

mod config;
mod frontmatter;
mod harness;
mod loader;
mod report;
mod runner;

use std::env;
use std::fs;
use std::io;
use std::process::ExitCode;

use config::{Config, HELP, Options};
use report::{Baseline, Execution, Outcome, directory_buckets};

fn main() -> ExitCode {
    match command() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("qjs-test262: {error}");
            ExitCode::from(2)
        }
    }
}

fn command() -> Result<ExitCode, String> {
    let Some(options) = Options::parse(env::args().skip(1))? else {
        println!("{HELP}");
        return Ok(ExitCode::SUCCESS);
    };
    let config = Config::load(&options.config)?;
    if !cfg!(feature = "integrated-runtime") && options.limit != Some(0) {
        return Err("rebuild with --features integrated-runtime to execute tests".into());
    }
    let compare = options.diff_errors || options.fail_on_regression;
    let baseline = if compare || options.update_errors {
        read_baseline(&config, options.update_errors)?
    } else {
        Baseline::default()
    };
    let results = runner::run(&config, &options)?;
    let diff = compare.then(|| baseline.diff(&results.executions));
    for notice in &results.notices {
        println!("{notice}");
    }
    if config.verbose > 0 && !options.diff_errors {
        for execution in &results.executions {
            if let Some(failure) = execution.failure(options.phase_strict) {
                println!("{}", failure.row(&execution.key));
                if config.verbose == 1 {
                    for line in &execution.output {
                        println!("{line}");
                    }
                }
            }
            if config.verbose > 1 {
                for line in &execution.output {
                    println!("{line}");
                }
            }
        }
    }
    if let Some(diff) = &diff {
        if options.diff_errors {
            for (key, failure) in &diff.new {
                println!("{} (new)", failure.row(key));
            }
            for (key, old, current) in &diff.changed {
                println!("{} (previous)", old.row(key));
                println!("{} (changed)", current.row(key));
            }
            for (key, failure) in &diff.fixed {
                println!("{} (fixed)", failure.row(key));
            }
        }
        eprintln!(
            "Errors: {} new, {} changed, {} fixed",
            diff.new.len(),
            diff.changed.len(),
            diff.fixed.len()
        );
    }
    if options.summary_by_dir {
        for (directory, count) in directory_buckets(&results.executions, options.phase_strict) {
            println!("{count:>6}  {directory}");
        }
    }
    if options.memory > 0 {
        print_memory(&results.executions, options.memory > 1);
    }
    let total = results.executions.len();
    let failed = results
        .executions
        .iter()
        .filter(|execution| execution.failure(false).is_some())
        .count();
    let phase_failed = results
        .executions
        .iter()
        .filter(|execution| execution.failure(true).is_some())
        .count();
    eprintln!(
        "Result: {failed}/{total} errors, {} excluded, {} skipped",
        results.excluded, results.skipped
    );
    eprintln!("Phase-strict: {phase_failed}/{total} errors");
    if options.update_errors {
        let path = config
            .error_file
            .as_ref()
            .ok_or("--update-errors requires config.errorfile")?;
        if results
            .executions
            .iter()
            .any(|run| matches!(run.outcome, Outcome::HostFailure(_)))
        {
            return Err(
                "error file left unchanged because a host failure prevented a test from running"
                    .into(),
            );
        }
        baseline
            .update(
                &results.executions,
                path,
                options.filter.is_some() || options.limit.is_some(),
            )
            .map_err(|error| format!("{}: {error}", path.display()))?;
    }
    let regression = diff.is_some_and(|diff| !diff.new.is_empty() || !diff.changed.is_empty());
    Ok(if options.fail_on_regression && regression {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

fn read_baseline(config: &Config, updating: bool) -> Result<Baseline, String> {
    let Some(path) = &config.error_file else {
        return Ok(Baseline::default());
    };
    match fs::read_to_string(path) {
        Ok(source) => {
            Baseline::parse(&source).map_err(|error| format!("{}: {error}", path.display()))
        }
        Err(error) if updating && error.kind() == io::ErrorKind::NotFound => {
            Ok(Baseline::default())
        }
        Err(error) => Err(format!("{}: {error}", path.display())),
    }
}

fn print_memory(executions: &[Execution], range: bool) {
    let mut samples = 0;
    let mut total = 0u128;
    let mut min: Option<(u64, &str)> = None;
    let mut max: Option<(u64, &str)> = None;
    for execution in executions {
        if let Some(bytes) = execution.heap_bytes {
            samples += 1;
            total += u128::from(bytes);
            let sample = (bytes, execution.key.path.as_str());
            min = Some(min.map_or(sample, |edge| edge.min(sample)));
            max = Some(max.map_or(sample, |edge| edge.max(sample)));
        }
    }
    if let Some(mean) = total.checked_div(samples) {
        println!("Mean retained engine heap: {mean} bytes across {samples} executions");
    }
    if range && let (Some((min, min_file)), Some((max, max_file))) = (min, max) {
        println!("Minimum: {min} bytes ({min_file})");
        println!("Maximum: {max} bytes ({max_file})");
    }
}

#[cfg(test)]
mod test_support {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    pub(crate) struct Scratch(pub PathBuf);

    impl Scratch {
        pub(crate) fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            loop {
                let dir = std::env::temp_dir().join(format!(
                    "test262-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
                match fs::create_dir(&dir) {
                    Ok(()) => return Self(dir),
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(error) => panic!("{}: {error}", dir.display()),
                }
            }
        }

        pub(crate) fn write(&self, relative: &str, contents: impl AsRef<[u8]>) -> PathBuf {
            let path = self.0.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, contents).unwrap();
            path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}
