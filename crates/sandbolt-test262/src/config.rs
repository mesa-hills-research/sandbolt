use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use crate::frontmatter::Metadata;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Mode {
    #[default]
    DefaultSloppy,
    DefaultStrict,
    Sloppy,
    Strict,
    Both,
}

impl Mode {
    pub(crate) fn variants(self, metadata: &Metadata) -> &'static [bool] {
        let sloppy = metadata.flag("noStrict") || metadata.flag("raw");
        let strict = metadata.flag("onlyStrict");
        match self {
            Self::DefaultSloppy if strict => &[true],
            Self::DefaultStrict if !sloppy => &[true],
            Self::DefaultSloppy | Self::DefaultStrict => &[false],
            Self::Strict if !sloppy => &[true],
            Self::Sloppy if !strict => &[false],
            Self::Both if metadata.flag("module") => &[false],
            Self::Both if sloppy && strict => &[],
            Self::Both if sloppy => &[false],
            Self::Both if strict => &[true],
            Self::Both => &[false, true],
            _ => &[],
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct Config {
    pub root: PathBuf,
    pub mode: Mode,
    pub local: bool,
    pub verbose: usize,
    pub skip_async: bool,
    pub skip_module: bool,
    pub harness: Option<PathBuf>,
    pub error_file: Option<PathBuf>,
    pub harness_excludes: BTreeSet<String>,
    pub features: BTreeSet<String>,
    pub skip_features: BTreeSet<String>,
    pub directories: Vec<PathBuf>,
    pub tests: Vec<PathBuf>,
    exclude_files: BTreeSet<PathBuf>,
    exclude_dirs: Vec<(PathBuf, bool)>,
}

#[derive(Clone, Copy, Default)]
enum Section {
    #[default]
    Other,
    Config,
    Exclude,
    Features,
    Tests,
}

impl Config {
    pub(crate) fn load(path: &Path) -> Result<Self, String> {
        let cwd = env::current_dir().map_err(|error| error.to_string())?;
        let path = normalize(&cwd.join(path));
        let mut config = Self {
            root: path.parent().unwrap_or(&cwd).to_owned(),
            ..Self::default()
        };
        let text = read(&path)?;
        let mut section = Section::Other;
        for (index, line) in text.lines().enumerate() {
            let Some(line) = entry(line) else { continue };
            if let Some(header) = line.strip_prefix('[') {
                section = match header.split(']').next().unwrap_or_default() {
                    "config" => Section::Config,
                    "exclude" => Section::Exclude,
                    "features" => Section::Features,
                    "tests" => Section::Tests,
                    _ => Section::Other,
                };
                continue;
            }
            let (key, value) = line
                .split_once('=')
                .map_or((line, None), |(key, value)| (key, Some(value.trim())));
            match section {
                Section::Config => {
                    let value = value.ok_or_else(|| {
                        format!("{}:{}: expected name=value", path.display(), index + 1)
                    })?;
                    config
                        .setting(key, value)
                        .map_err(|error| format!("{}:{}: {error}", path.display(), index + 1))?;
                }
                Section::Exclude => config.exclude(&config.root.clone(), key),
                Section::Features => {
                    // The reference compares the entire value, including any inline comment.
                    let set = if matches!(value, None | Some("yes" | "!tcc")) {
                        &mut config.features
                    } else {
                        &mut config.skip_features
                    };
                    set.insert(key.into());
                }
                Section::Tests => config.tests.push(normalize(&config.root.join(key))),
                Section::Other => {}
            }
        }
        Ok(config)
    }

    fn setting(&mut self, key: &str, value: &str) -> Result<(), String> {
        match key {
            "local" => self.local = value == "yes",
            "testdir" => self.directories.push(normalize(&self.root.join(value))),
            "harnessdir" => self.harness = Some(normalize(&self.root.join(value))),
            "errorfile" => self.error_file = Some(normalize(&self.root.join(value))),
            "harnessexclude" => self.harness_excludes.extend(words(value)),
            "features" => self.features.extend(words(value)),
            "skip-features" => self.skip_features.extend(words(value)),
            "mode" => {
                self.mode = match value {
                    "default" | "default-nostrict" => Mode::DefaultSloppy,
                    "default-strict" => Mode::DefaultStrict,
                    "nostrict" => Mode::Sloppy,
                    "strict" => Mode::Strict,
                    "all" | "both" => Mode::Both,
                    _ => return Err(format!("unknown test mode {value}")),
                }
            }
            "strict" if matches!(value, "no" | "skip") => self.mode = Mode::Sloppy,
            "nostrict" if matches!(value, "no" | "skip") => self.mode = Mode::Strict,
            "strict" | "nostrict" => {}
            "async" => self.skip_async = value != "yes",
            "module" => self.skip_module = value != "yes",
            "verbose" => self.verbose = self.verbose.max(value.matches("yes").count()),
            "excludefile" => {
                let path = normalize(&self.root.join(value));
                let base = path.parent().unwrap_or(Path::new(""));
                for name in read(&path)?.lines().filter_map(entry) {
                    self.exclude(base, name);
                }
            }
            _ => self.exclude(&self.root.clone(), key),
        }
        Ok(())
    }

    fn exclude(&mut self, base: &Path, name: &str) {
        if name.ends_with('/') {
            let (name, include) = name.strip_prefix('!').map_or((name, false), |p| (p, true));
            self.exclude_dirs
                .push((normalize(&base.join(name)), include));
        } else {
            self.exclude_files.insert(normalize(&base.join(name)));
        }
    }

    pub(crate) fn excluded(&self, path: &Path) -> bool {
        let mut include = 0;
        let mut exclude = 0;
        for (dir, is_include) in &self.exclude_dirs {
            if path.starts_with(dir) {
                let depth = dir.components().count();
                let bound = if *is_include {
                    &mut include
                } else {
                    &mut exclude
                };
                *bound = (*bound).max(depth);
            }
        }
        self.exclude_files.contains(path) || exclude > include
    }

    pub(crate) fn name(&self, path: &Path) -> String {
        path.strip_prefix(&self.root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/")
    }
}

pub(crate) fn normalize(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if result
                    .components()
                    .next_back()
                    .is_some_and(|p| matches!(p, Component::Normal(_)))
                {
                    result.pop();
                } else if !result.has_root() {
                    result.push("..");
                }
            }
            component => result.push(component.as_os_str()),
        }
    }
    result
}

pub(crate) fn read(path: &Path) -> Result<String, String> {
    fs::read_to_string(path).map_err(|error| format!("{}: {error}", path.display()))
}

fn entry(line: &str) -> Option<&str> {
    let line = line.trim();
    (!line.is_empty() && !line.starts_with(['#', ';'])).then_some(line)
}

fn words(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|c: char| c.is_whitespace() || c == ',')
        .filter(|word| !word.is_empty())
        .map(str::to_owned)
}

pub(crate) struct Options {
    pub config: PathBuf,
    pub all_modes: bool,
    pub memory: usize,
    pub filter: Option<String>,
    pub threads: usize,
    pub timeout: Duration,
    pub phase_strict: bool,
    pub diff_errors: bool,
    pub summary_by_dir: bool,
    pub update_errors: bool,
    pub fail_on_regression: bool,
    pub limit: Option<usize>,
}

pub(crate) const HELP: &str = "Usage: sandbolt-test262 [options]\n\
  -c FILE              Read test262.conf or tests.conf (default: test262.conf)\n\
  -a                   Run strict and non-strict variants\n\
  -m                   Show retained engine heap usage (-mm adds min/max)\n\
  -f FILTER            Run paths containing FILTER\n\
  -j THREADS           Set the worker count (default: available cores)\n\
  --timeout-ms MS       Limit each execution (default: 10000)\n\
  --phase-strict       Show phase-strict failure details and summaries\n\
  --diff-errors        Compare failures with the configured error file\n\
  --summary-by-dir     Group failures by directory\n\
  --update-errors      Write the current failures to the configured error file\n\
  --fail-on-regression Exit nonzero on new or changed failures\n\
  --limit N            Run at most N matching test files\n\
  -h, --help           Show this help";

impl Options {
    pub(crate) fn parse(args: impl IntoIterator<Item = String>) -> Result<Option<Self>, String> {
        let mut options = Self {
            config: "test262.conf".into(),
            all_modes: false,
            memory: 0,
            filter: None,
            threads: std::thread::available_parallelism().map_or(1, usize::from),
            timeout: Duration::from_secs(10),
            phase_strict: false,
            diff_errors: false,
            summary_by_dir: false,
            update_errors: false,
            fail_on_regression: false,
            limit: None,
        };
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            let mut value = || args.next().ok_or_else(|| format!("{arg} needs a value"));
            match arg.as_str() {
                "-h" | "--help" => return Ok(None),
                "-c" => options.config = value()?.into(),
                "-a" => options.all_modes = true,
                "-m" => options.memory += 1,
                "-mm" => options.memory += 2,
                "-f" => options.filter = Some(value()?),
                "-j" => options.threads = number(&arg, &value()?, false)?,
                "--timeout-ms" => {
                    options.timeout = Duration::from_millis(number(&arg, &value()?, false)?)
                }
                "--limit" => options.limit = Some(number(&arg, &value()?, true)?),
                "--phase-strict" => options.phase_strict = true,
                "--diff-errors" => options.diff_errors = true,
                "--summary-by-dir" => options.summary_by_dir = true,
                "--update-errors" | "-u" => options.update_errors = true,
                "--fail-on-regression" => options.fail_on_regression = true,
                _ => return Err(format!("unknown option {arg}. See --help.")),
            }
        }
        Ok(Some(options))
    }
}

fn number<T: std::str::FromStr + Default + PartialOrd>(
    arg: &str,
    value: &str,
    zero: bool,
) -> Result<T, String> {
    value
        .parse::<T>()
        .ok()
        .filter(|n| zero || *n > T::default())
        .ok_or_else(|| format!("invalid value for {arg}: {value}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::Scratch;

    #[test]
    fn config_paths_features_and_exclusions() {
        let tree = Scratch::new();
        let path = tree.write("config/test262.conf", "[config]\nmode=default-strict\nlocal=yes\nasync=no\nmodule=no\nharnessdir=harness\nharnessexclude=a.js,b.js\nfeatures=a b\nskip-features=old\nexcludefile=lists/skips.txt\nverbose=yes yes\ntestdir=cases\nerrorfile=errors.txt\n[features]\npresent\nabsent=skip\nAtomics=!tcc\nWithComment=!tcc # reason\n[exclude]\ncases/\n!cases/keep/\ncases/keep/specific.js\n[tests]\ncases/keep/one.js\n");
        tree.write(
            "config/lists/skips.txt",
            "# comment\n../cases/keep/two.js\n",
        );
        let config = Config::load(&path).unwrap();
        assert!(config.local && config.skip_async && config.skip_module);
        assert_eq!(config.mode, Mode::DefaultStrict);
        assert_eq!(
            config.features,
            ["Atomics", "a", "b", "present"].map(String::from).into()
        );
        assert_eq!(
            config.skip_features,
            ["WithComment", "absent", "old"].map(String::from).into()
        );
        assert_eq!(config.harness_excludes.len(), 2);
        assert_eq!(config.verbose, 2);
        assert_eq!(config.error_file, Some(config.root.join("errors.txt")));
        assert_eq!(config.directories, [config.root.join("cases")]);
        assert_eq!(config.name(&config.tests[0]), "cases/keep/one.js");
        for path in [
            "cases/skip.js",
            "cases/keep/specific.js",
            "cases/keep/two.js",
        ] {
            assert!(config.excluded(&config.root.join(path)), "{path}");
        }
        assert!(!config.excluded(&config.tests[0]));
        assert!(!config.excluded(&config.root.join("cases-too/one.js")));
    }

    #[test]
    fn cli() {
        let parse = |input: &str| Options::parse(input.split_whitespace().map(str::to_owned));
        let options = parse("-c another.conf -a -m -f check -j 4 --timeout-ms 29 --phase-strict --diff-errors --summary-by-dir --update-errors --fail-on-regression --limit 0").unwrap().unwrap();
        assert_eq!(options.config, Path::new("another.conf"));
        assert!(options.all_modes);
        assert_eq!(options.memory, 1);
        assert_eq!(options.filter.as_deref(), Some("check"));
        assert_eq!(options.threads, 4);
        assert_eq!(options.timeout, Duration::from_millis(29));
        assert!(options.phase_strict && options.diff_errors && options.summary_by_dir);
        assert!(options.update_errors && options.fail_on_regression);
        assert_eq!(options.limit, Some(0));
        assert!(parse("-j 0").is_err());
        assert!(parse("-f").is_err());
        assert!(parse("--help").unwrap().is_none());
    }

    #[test]
    #[ignore = "set QUICKJS_REF to the reference checkout"]
    fn pinned_configs() {
        let root = PathBuf::from(env::var_os("QUICKJS_REF").unwrap());
        let config = Config::load(&root.join("test262.conf")).unwrap();
        assert_eq!(config.mode, Mode::DefaultSloppy);
        assert!(!config.local && !config.skip_async && !config.skip_module);
        assert_eq!(config.features.len(), 168);
        assert_eq!(config.skip_features.len(), 30);
        assert_eq!(config.exclude_files.len(), 17);
        assert_eq!(config.exclude_dirs.len(), 3);
        assert!(config.tests.is_empty());
        let local = Config::load(&root.join("tests.conf")).unwrap();
        assert!(local.local);
        assert_eq!(local.features, ["skip-if-tcc".into()].into());
        assert!(local.skip_features.is_empty());
        assert_eq!(local.exclude_files.len(), 9);
        assert_eq!(local.exclude_dirs.len(), 0);
        assert!(local.tests.is_empty());
    }
}
