use qjs_bytecode::verify_image;
use qjs_compiler::{CompileError, compile_source};
use qjs_syntax::{ParseGoal, ParseOptions, SourceText};
use std::{collections::BTreeMap, fs, path::Path};

fn walk(dir: &Path, paths: &mut Vec<std::path::PathBuf>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            walk(&path, paths);
        } else if path.extension().is_some_and(|e| e == "js")
            && !path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .contains("_FIXTURE")
        {
            paths.push(path);
        }
    }
}

fn in_list(meta: &str, field: &str, item: &str) -> bool {
    let mut lines = meta.lines();
    let Some(tail) = lines.find_map(|line| {
        line.strip_prefix(field)
            .and_then(|rest| rest.strip_prefix(':'))
    }) else {
        return false;
    };
    if let Some(tail) = tail.trim().strip_prefix('[') {
        return tail
            .trim_end_matches(']')
            .split(',')
            .any(|word| word.trim() == item);
    }
    lines
        .take_while(|line| line.starts_with(char::is_whitespace))
        .any(|line| line.trim().strip_prefix("- ") == Some(item))
}

#[test]
#[ignore = "Set TEST262_ROOT to a test262 checkout"]
fn compile_test262() {
    let root = std::env::var_os("TEST262_ROOT").expect("Set TEST262_ROOT to a test262 checkout");
    let mut paths = Vec::new();
    walk(&Path::new(&root).join("test"), &mut paths);
    paths.sort();
    let mut verified = 0;
    let mut excluded = 0;
    let mut syntax = BTreeMap::<String, (u32, String)>::new();
    let mut lowering = BTreeMap::<String, (u32, String)>::new();
    for path in &paths {
        let bytes = fs::read(path).unwrap();
        let text = match std::str::from_utf8(&bytes) {
            Ok(text) => text,
            Err(_) => {
                excluded += 1;
                continue;
            }
        };
        let meta = text
            .split_once("/*---")
            .and_then(|(_, s)| s.split_once("---*/"))
            .map_or("", |(meta, _)| meta);
        if meta
            .lines()
            .any(|l| matches!(l.trim(), "phase: parse" | "phase: early"))
        {
            excluded += 1;
            continue;
        }
        if [
            "decorators",
            "import-defer",
            "source-phase-imports",
            "source-phase-imports-module-source",
        ]
        .iter()
        .any(|proposal| in_list(meta, "features", proposal))
        {
            excluded += 1;
            continue;
        }
        let flag = |value| in_list(meta, "flags", value);
        let goal = if flag("module") {
            ParseGoal::Module
        } else {
            ParseGoal::Script
        };
        let source = SourceText::from_utf8(path.to_string_lossy(), text);
        let result = compile_source(
            &source,
            ParseOptions {
                goal,
                strict: flag("onlyStrict"),
                max_bytes: 512 * 1024 * 1024,
                ..ParseOptions::default()
            },
        );
        match result {
            Ok(image) => {
                verify_image(&image).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
                verified += 1;
            }
            Err(CompileError::Syntax { message, line, col }) => {
                let value = syntax
                    .entry(message.clone())
                    .or_insert_with(|| (0, format!("{}:{line}:{col}: {message}", path.display())));
                value.0 += 1;
            }
            Err(error) => {
                let key = match &error {
                    CompileError::Bytecode(message) => message
                        .split_once(": ")
                        .map_or(message.as_str(), |(_, reason)| {
                            reason.split(" in ").next().unwrap_or(reason)
                        })
                        .to_owned(),
                    _ => error.to_string(),
                };
                let value = lowering
                    .entry(key)
                    .or_insert_with(|| (0, format!("{}: {error}", path.display())));
                value.0 += 1;
            }
        }
    }
    let parse_errors = syntax.values().map(|(n, _)| n).sum::<u32>();
    eprintln!(
        "{} files, {excluded} excluded, {verified} compiled and verified, {parse_errors} syntax errors, {} compiler errors",
        paths.len(),
        lowering.values().map(|(n, _)| n).sum::<u32>()
    );
    for (n, path) in syntax.values() {
        eprintln!("{n:4} {path}");
    }
    for (n, path) in lowering.values() {
        eprintln!("{n:4} {path}");
    }
    assert!(verified > 0);
    assert!(lowering.is_empty());
    assert_eq!(paths.len(), verified + excluded + parse_errors as usize);
    assert_eq!(parse_errors, 0);
}
