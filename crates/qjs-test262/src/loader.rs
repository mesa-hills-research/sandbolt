use std::fs;
use std::path::{Path, PathBuf};

use qjs_runtime::api::{ModuleLoader, ResolvedModule};

use crate::config::normalize;

pub(crate) struct Fixtures {
    root: PathBuf,
    entry: PathBuf,
    local: bool,
}

impl Fixtures {
    pub(crate) fn new(root: &Path, entry: &Path, local: bool) -> Self {
        Self {
            root: root.to_owned(),
            entry: entry.to_owned(),
            local,
        }
    }

    fn name(&self, path: &Path) -> String {
        path.strip_prefix(&self.root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/")
    }
}

impl ModuleLoader for Fixtures {
    fn resolve(
        &mut self,
        referrer: &str,
        specifier: &str,
        module_type: Option<&str>,
    ) -> Result<ResolvedModule, String> {
        if self.local && module_type.is_none() && specifier == "qjs:std" {
            return Ok(ResolvedModule::Source {
                name: specifier.into(),
                source:
                    "export const gc = globalThis.gc; export const evalScript = $262.evalScript;"
                        .into(),
            });
        }
        let kind = match module_type {
            None if specifier.ends_with(".json") => Some("json"),
            None => None,
            kind @ Some("json" | "text" | "bytes") => kind,
            Some(kind) => return Err(format!("unsupported module type: '{kind}'")),
        };
        let referrer = if referrer.starts_with('<') {
            self.entry.clone()
        } else {
            self.root.join(referrer)
        };
        let base = if specifier.starts_with('.') || !specifier.contains('/') {
            referrer.parent().unwrap_or(&self.root)
        } else {
            &self.root
        };
        let path = normalize(&base.join(specifier));
        let name = self.name(&path);
        let bytes =
            fs::read(&path).map_err(|_| format!("could not load module filename '{name}'"))?;
        match kind {
            Some("bytes") => Ok(ResolvedModule::Bytes { name, bytes }),
            Some("text") => Ok(ResolvedModule::Text {
                name,
                text: String::from_utf8_lossy(&bytes).into(),
            }),
            _ => {
                let source = String::from_utf8(bytes)
                    .map_err(|_| format!("invalid UTF-8 in module '{name}'"))?;
                if kind == Some("json") {
                    Ok(ResolvedModule::Json { name, source })
                } else {
                    Ok(ResolvedModule::Source { name, source })
                }
            }
        }
    }
}

pub(crate) fn is_test(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.ends_with(".js") && !name.ends_with("_FIXTURE.js"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::Scratch;

    #[test]
    fn resolves_types_and_normalizes_identity() {
        let tree = Scratch::new();
        let entry = tree.write("s/main.js", "");
        tree.write("s/dep_FIXTURE.js", "export const x = 1;");
        tree.write("values.json", "{\"value\": 1}");
        tree.write("s/text_FIXTURE.txt", "hello\n");
        tree.write("s/bytes_FIXTURE.bytes", [0, 128, 255]);
        let mut loader = Fixtures::new(&tree.0, &entry, false);
        assert!(
            matches!(loader.resolve("s/main.js", "./a/../dep_FIXTURE.js", None), Ok(ResolvedModule::Source { name, .. }) if name == "s/dep_FIXTURE.js")
        );
        assert!(
            matches!(loader.resolve("s/main.js", "dep_FIXTURE.js", None), Ok(ResolvedModule::Source { name, .. }) if name == "s/dep_FIXTURE.js")
        );
        assert!(
            matches!(loader.resolve("s/main.js", "../values.json", None), Ok(ResolvedModule::Json { name, .. }) if name == "values.json")
        );
        assert!(
            matches!(loader.resolve("s/main.js", "../values.json", Some("text")), Ok(ResolvedModule::Text { text, .. }) if text == "{\"value\": 1}")
        );
        assert!(
            matches!(loader.resolve("s/main.js", "./text_FIXTURE.txt", Some("text")), Ok(ResolvedModule::Text { text, .. }) if text == "hello\n")
        );
        assert!(
            matches!(loader.resolve("s/main.js", "./bytes_FIXTURE.bytes", Some("bytes")), Ok(ResolvedModule::Bytes { bytes, .. }) if bytes == [0, 128, 255])
        );
        assert_eq!(
            loader.resolve("s/main.js", "./none.js", None).unwrap_err(),
            "could not load module filename 's/none.js'"
        );
        assert_eq!(
            loader
                .resolve("s/main.js", "./none.js", Some("other"))
                .unwrap_err(),
            "unsupported module type: 'other'"
        );
        assert!(is_test(&entry));
        assert!(!is_test(Path::new("dep_FIXTURE.js")));
        assert!(!is_test(Path::new("file.json")));
    }
}
