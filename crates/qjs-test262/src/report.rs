use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::{self, Write as _};
use std::path::Path;

use crate::frontmatter::{Negative, Phase};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Exception {
    pub phase: Phase,
    pub name: Option<String>,
    pub message: String,
    pub line: u32,
}

#[derive(Debug)]
pub(crate) enum Outcome {
    Complete,
    Exception(Exception),
    HostFailure(String),
}

pub(crate) struct Execution {
    pub key: Key,
    pub negative: Option<Negative>,
    pub outcome: Outcome,
    pub heap_bytes: Option<u64>,
    pub output: Vec<String>,
    pub test_line: u32,
}

impl Execution {
    pub(crate) fn failure(&self, phase_strict: bool) -> Option<Failure> {
        let host_line = self.test_line;
        let (line, message) = match (&self.negative, &self.outcome) {
            (None, Outcome::Complete) => return None,
            (_, Outcome::HostFailure(message)) => (host_line, message.clone()),
            (None, Outcome::Exception(error)) => (error.line, error.message.clone()),
            (Some(expected), Outcome::Complete) => (
                host_line,
                if phase_strict {
                    format!("expected {} during {}", expected.error_type, expected.phase)
                } else {
                    "expected error".into()
                },
            ),
            (Some(expected), Outcome::Exception(error)) => {
                let upstream_name = error.message.split(':').next().unwrap_or_default();
                if phase_strict {
                    let actual = error.name.as_deref().unwrap_or("non-error value");
                    if error.phase == expected.phase && actual == expected.error_type {
                        return None;
                    }
                    (
                        error.line,
                        format!(
                            "expected {} during {}, got {actual} during {} ({})",
                            expected.error_type, expected.phase, error.phase, error.message
                        ),
                    )
                } else {
                    if upstream_name == expected.error_type {
                        return None;
                    }
                    (
                        error.line,
                        format!("unexpected error type: {}", error.message),
                    )
                }
            }
        };
        Some(Failure { line, message })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Key {
    pub path: String,
    pub strict: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Failure {
    pub line: u32,
    pub message: String,
}

impl Failure {
    pub(crate) fn row(&self, key: &Key) -> String {
        let mode = if key.strict { "strict mode: " } else { "" };
        format!("{}:{}: {mode}{}", key.path, self.line, self.message)
    }
}

#[derive(Default)]
pub(crate) struct Baseline(BTreeMap<Key, Failure>);

impl Baseline {
    pub(crate) fn parse(source: &str) -> Result<Self, String> {
        let mut baseline = Self::default();
        let mut current: Option<(Key, Failure)> = None;
        for (index, text) in source.lines().enumerate() {
            if let Some((key, failure)) = parse_row(text) {
                if let Some((key, failure)) = current.replace((key, failure)) {
                    baseline.0.insert(key, failure);
                }
            } else if let Some((_, failure)) = &mut current {
                failure.message.push('\n');
                failure.message.push_str(text);
            } else if !text.trim().is_empty() {
                return Err(format!("invalid error row at line {}", index + 1));
            }
        }
        if let Some((key, failure)) = current {
            baseline.0.insert(key, failure);
        }
        Ok(baseline)
    }

    pub(crate) fn diff(&self, executions: &[Execution]) -> Diff {
        let mut diff = Diff::default();
        for execution in executions {
            let key = &execution.key;
            let previous = self.0.get(key);
            let current = execution.failure(false);
            match (previous, current) {
                (None, Some(failure)) => diff.new.push((key.clone(), failure)),
                (Some(old), Some(failure)) => {
                    // Upstream compares exception text only. A known failing negative
                    // is already accounted for, even when it now fails differently.
                    if execution.negative.is_none() && old.message != failure.message {
                        diff.changed.push((key.clone(), old.clone(), failure));
                    }
                }
                (Some(old), None) => {
                    let fixed = match (&execution.negative, &execution.outcome) {
                        (Some(_), Outcome::Exception(error)) => {
                            old.message == "expected error"
                                || old.message.starts_with("unexpected error type:")
                                || old.message == error.message
                        }
                        _ => true,
                    };
                    if fixed {
                        diff.fixed.push((key.clone(), old.clone()));
                    }
                }
                _ => {}
            }
        }
        diff
    }

    pub(crate) fn update(
        mut self,
        executions: &[Execution],
        path: &Path,
        partial: bool,
    ) -> io::Result<()> {
        if !partial {
            self.0.clear();
        }
        for execution in executions {
            if let Some(failure) = execution.failure(false) {
                self.0.insert(execution.key.clone(), failure);
            } else {
                self.0.remove(&execution.key);
            }
        }
        let mut text = String::new();
        let mut entries: Vec<_> = self.0.iter().collect();
        entries.sort_by(|a, b| {
            crate::runner::natural_cmp(&a.0.path, &b.0.path).then(a.0.strict.cmp(&b.0.strict))
        });
        for (key, failure) in entries {
            writeln!(&mut text, "{}", failure.row(key)).unwrap();
        }
        replace(path, &text)
    }
}

fn parse_row(text: &str) -> Option<(Key, Failure)> {
    let (path, rest) = text.split_once(".js:")?;
    let (line, rest) = rest.split_once(':')?;
    let line = line.parse().ok()?;
    let rest = rest.trim_start_matches(' ');
    let (strict, message) = rest
        .strip_prefix("strict mode: ")
        .map_or((false, rest), |rest| (true, rest));
    let message = message
        .strip_prefix("unexpected error: ")
        .unwrap_or(message);
    Some((
        Key {
            path: format!("{path}.js"),
            strict,
        },
        Failure {
            line,
            message: message.into(),
        },
    ))
}

fn replace(path: &Path, source: &str) -> io::Result<()> {
    let suffix = format!("test262-{}.tmp", std::process::id());
    let temporary = path.with_extension(suffix);
    let mut file = File::create_new(&temporary)?;
    let result = (|| {
        file.write_all(source.as_bytes())?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[derive(Default)]
pub(crate) struct Diff {
    pub new: Vec<(Key, Failure)>,
    pub changed: Vec<(Key, Failure, Failure)>,
    pub fixed: Vec<(Key, Failure)>,
}

pub(crate) fn directory_buckets(
    executions: &[Execution],
    phase_strict: bool,
) -> Vec<(&str, usize)> {
    let mut counts = BTreeMap::new();
    for execution in executions {
        if execution.failure(phase_strict).is_some() {
            let directory = execution
                .key
                .path
                .rsplit_once('/')
                .map_or(".", |(dir, _)| dir);
            *counts.entry(directory).or_insert(0) += 1;
        }
    }
    let mut counts: Vec<_> = counts.into_iter().collect();
    counts.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    counts
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::Scratch;

    fn attempt(path: &str, strict: bool, negative: Option<Phase>, outcome: Outcome) -> Execution {
        Execution {
            key: Key {
                path: path.into(),
                strict,
            },
            negative: negative.map(|phase| Negative {
                phase,
                error_type: "SyntaxError".into(),
            }),
            outcome,
            heap_bytes: None,
            output: Vec::new(),
            test_line: 1,
        }
    }

    fn exception(phase: Phase, name: Option<&str>, message: &str) -> Outcome {
        Outcome::Exception(Exception {
            phase,
            name: name.map(str::to_owned),
            message: message.into(),
            line: 10,
        })
    }

    #[test]
    fn upstream_ignores_phase_and_uses_string_prefix() {
        let wrong_phase = attempt(
            "one.js",
            false,
            Some(Phase::Parse),
            exception(Phase::Runtime, Some("SyntaxError"), "SyntaxError: bad"),
        );
        let thrown_string = attempt(
            "two.js",
            false,
            Some(Phase::Parse),
            exception(Phase::Parse, None, "SyntaxError: bad"),
        );
        let exact = attempt(
            "three.js",
            false,
            Some(Phase::Parse),
            exception(Phase::Parse, Some("SyntaxError"), "SyntaxError: bad"),
        );
        let wrong_type = attempt(
            "four.js",
            false,
            Some(Phase::Parse),
            exception(Phase::Parse, Some("TypeError"), "TypeError: bad"),
        );
        let timeout = attempt(
            "five.js",
            false,
            Some(Phase::Parse),
            Outcome::HostFailure("timed out".into()),
        );
        for run in [&wrong_phase, &thrown_string] {
            assert!(run.failure(false).is_none());
            assert!(run.failure(true).is_some());
        }
        assert!(exact.failure(false).is_none() && exact.failure(true).is_none());
        for run in [wrong_type, timeout] {
            assert!(run.failure(false).is_some() && run.failure(true).is_some());
        }
    }

    #[test]
    fn compares_modes_and_preserves_untested_rows() {
        let source = "cases/a.js:2: TypeError: old\ncases/a.js:3: strict mode: expected error\ncases/b.js:4: unexpected error: SyntaxError: same \n  frame\ncases/d.js:2: unexpected error type: ReferenceError\n";
        let baseline = Baseline::parse(source).unwrap();
        let runs = [
            attempt(
                "cases/a.js",
                false,
                None,
                exception(Phase::Runtime, Some("TypeError"), "TypeError: new"),
            ),
            attempt(
                "cases/a.js",
                true,
                Some(Phase::Parse),
                exception(Phase::Parse, Some("SyntaxError"), "SyntaxError: bad"),
            ),
            attempt(
                "cases/b.js",
                false,
                None,
                exception(
                    Phase::Parse,
                    Some("SyntaxError"),
                    "SyntaxError: same \n  frame",
                ),
            ),
            attempt(
                "cases/c.js",
                false,
                Some(Phase::Resolution),
                Outcome::Complete,
            ),
            attempt("cases/d.js", false, Some(Phase::Runtime), Outcome::Complete),
        ];
        let diff = baseline.diff(&runs);
        assert_eq!(
            (diff.new.len(), diff.changed.len(), diff.fixed.len()),
            (1, 1, 1)
        );
        assert_eq!(diff.new[0].0.path, "cases/c.js");
        assert!(diff.fixed[0].0.strict);
        let tree = Scratch::new();
        let path = tree.write("known.txt", source);
        baseline.update(&runs[..2], &path, true).unwrap();
        let updated = Baseline::parse(&fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(updated.0.len(), 3);
        assert!(updated.diff(&runs[..3]).changed.is_empty());
    }

    #[test]
    fn buckets_count_failed_executions_in_each_mode() {
        let runs = [
            attempt(
                "b/x.js",
                false,
                None,
                exception(Phase::Runtime, None, "error"),
            ),
            attempt(
                "b/x.js",
                true,
                None,
                exception(Phase::Runtime, None, "error"),
            ),
            attempt(
                "a/x.js",
                false,
                Some(Phase::Parse),
                exception(Phase::Runtime, Some("SyntaxError"), "SyntaxError"),
            ),
            attempt("top.js", false, None, Outcome::Complete),
        ];
        assert_eq!(directory_buckets(&runs, false), [("b", 2)]);
        assert_eq!(directory_buckets(&runs, true), [("b", 2), ("a", 1)]);
    }

    #[test]
    #[ignore = "set QUICKJS_REF to the reference checkout"]
    fn pinned_errors() {
        let root = std::env::var_os("QUICKJS_REF").unwrap();
        let source = std::fs::read_to_string(Path::new(&root).join("test262_errors.txt")).unwrap();
        let baseline = Baseline::parse(&source).unwrap();
        assert_eq!(baseline.0.len(), 49);
        assert_eq!(baseline.0.keys().filter(|key| key.strict).count(), 14);
        assert_eq!(
            baseline
                .0
                .keys()
                .map(|key| &key.path)
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            35
        );
        assert_eq!(
            baseline
                .0
                .values()
                .filter(|failure| failure.message.starts_with("unexpected error type:"))
                .count(),
            2
        );
    }
}
