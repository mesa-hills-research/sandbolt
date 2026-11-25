use std::{
    collections::HashSet,
    env, fs,
    path::{Path, PathBuf},
};

use sandbolt_syntax::{ParseErrorKind, ParseGoal, ParseOptions, SourceText, parse};

#[derive(Default)]
struct Counts {
    total: usize,
    invalid_accepted: usize,
    valid_rejected: usize,
    wrong_error: usize,
    skipped: usize,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = env::args_os().skip(1).map(PathBuf::from);
    let config = fs::read_to_string(args.next().ok_or("Expected path to test262.conf")?)?;
    let args = args.collect::<Vec<_>>();
    if args.is_empty() {
        return Err("Expected at least one test directory".into());
    }
    let skipped = config
        .lines()
        .filter_map(|line| line.split_once('='))
        .filter(|(_, value)| value.split_whitespace().next() == Some("skip"))
        .map(|(name, _)| name.trim())
        .collect();
    let mut counts = Counts::default();
    for root in args {
        visit(&root, &skipped, &mut counts)?;
    }
    eprintln!(
        "{} parses, {} invalid accepted, {} valid rejected, {} wrong errors, {} files skipped",
        counts.total,
        counts.invalid_accepted,
        counts.valid_rejected,
        counts.wrong_error,
        counts.skipped
    );
    if counts.invalid_accepted != 0 || counts.valid_rejected != 0 || counts.wrong_error != 0 {
        std::process::exit(1);
    }
    Ok(())
}

fn visit(path: &Path, skip: &HashSet<&str>, counts: &mut Counts) -> std::io::Result<()> {
    if path.is_dir() {
        let mut paths = fs::read_dir(path)?
            .map(|entry| entry.map(|item| item.path()))
            .collect::<std::io::Result<Vec<_>>>()?;
        paths.sort();
        for path in paths {
            visit(&path, skip, counts)?;
        }
    } else if path.extension().is_some_and(|ext| ext == "js")
        && !path.to_string_lossy().ends_with("_FIXTURE.js")
    {
        let bytes = fs::read(path)?;
        let Ok(text) = std::str::from_utf8(&bytes) else {
            counts.skipped += 1;
            return Ok(());
        };
        let Some((meta, _)) = text
            .split_once("/*---")
            .and_then(|(_, rest)| rest.split_once("---*/"))
        else {
            counts.skipped += 1;
            return Ok(());
        };
        if metadata_list(meta, "features").any(|feature| skip.contains(feature)) {
            counts.skipped += 1;
            return Ok(());
        }
        let flags: HashSet<_> = metadata_list(meta, "flags").collect();
        let negative = meta
            .lines()
            .any(|line| matches!(line.trim(), "phase: parse" | "phase: early"));
        let module = flags.contains("module");
        let modes: &[bool] = if module || flags.contains("onlyStrict") {
            &[true]
        } else if flags.contains("noStrict") || flags.contains("raw") {
            &[false]
        } else {
            &[false, true]
        };
        let source = SourceText::from_utf8(path.display().to_string(), text);
        for &strict in modes {
            counts.total += 1;
            let goal = if module {
                ParseGoal::Module
            } else {
                ParseGoal::Script
            };
            let result = parse(
                &source,
                ParseOptions {
                    goal,
                    strict,
                    ..ParseOptions::default()
                },
            );
            let suffix = if module {
                "module"
            } else if strict {
                "strict"
            } else {
                "sloppy"
            };
            match (result, negative) {
                (Ok(_), true) => {
                    counts.invalid_accepted += 1;
                    println!("accepted {} ({suffix})", path.display());
                }
                (Err(e), false) => {
                    counts.valid_rejected += 1;
                    println!("rejected {} ({suffix}) {e}", path.display());
                }
                (Err(e), true) if e.kind != ParseErrorKind::Syntax => {
                    counts.wrong_error += 1;
                    println!("wrong error {} ({suffix}) {e}", path.display());
                }
                _ => (),
            }
        }
    }
    Ok(())
}

fn metadata_list<'a>(meta: &'a str, key: &str) -> impl Iterator<Item = &'a str> {
    meta.lines()
        .find_map(|line| {
            line.split_once(':')
                .filter(|(name, _)| *name == key)
                .map(|(_, list)| list)
        })
        .and_then(|list| list.trim().strip_prefix('[')?.strip_suffix(']'))
        .into_iter()
        .flat_map(|list| list.split(',').map(str::trim))
}
