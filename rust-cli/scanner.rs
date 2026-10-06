use crate::config::Config;
use ignore::WalkBuilder;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs;
use std::path::Path;

/// A source file that belongs to the project. This is the denominator for coverage:
/// every one of these should end up with semantic data from some indexer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceFile {
    /// Path relative to the project root, always with `/` separators.
    pub path: String,
    pub language: String,
    pub size: u64,
    pub hash: String,
    pub lines: i64,
}

pub struct ScanResult {
    pub files: Vec<SourceFile>,
    /// Relative paths of files whose name is one of the requested marker names
    /// (`Cargo.toml`, `tsconfig.json`, ...), used to find project roots for indexers.
    pub markers: Vec<String>,
    pub warnings: Vec<String>,
}

/// Compiles gitignore-style patterns (`dir/`, `*.ext`, `/rooted`, `!negated`).
pub fn build_matcher(root: &Path, patterns: &[String]) -> Result<Gitignore, ignore::Error> {
    let mut builder = GitignoreBuilder::new(root);
    for pattern in patterns {
        builder.add_line(None, pattern)?;
    }
    builder.build()
}

pub fn relative_string(rel: &Path) -> String {
    rel.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// Walks the project, pruning ignored directories as it goes (so `node_modules` is never
/// traversed). Unreadable entries become warnings instead of aborting the scan.
pub fn scan_project(
    root: &Path,
    config: &Config,
    language_of: &dyn Fn(&str) -> Option<String>,
    marker_names: &HashSet<String>,
) -> Result<ScanResult, Box<dyn std::error::Error>> {
    let matcher = build_matcher(root, &config.ignore_patterns)?;

    let mut walker = WalkBuilder::new(root);
    walker
        .follow_links(false)
        .hidden(true)
        .ignore(false)
        .git_global(false)
        .git_ignore(config.respect_gitignore)
        .git_exclude(config.respect_gitignore)
        .require_git(false);
    {
        let root = root.to_path_buf();
        walker.filter_entry(move |entry| {
            let rel = entry.path().strip_prefix(&root).unwrap_or(entry.path());
            if rel.as_os_str().is_empty() {
                return true;
            }
            let is_dir = entry.file_type().is_some_and(|t| t.is_dir());
            !matcher.matched(rel, is_dir).is_ignore()
        });
    }

    let mut files = Vec::new();
    let mut markers = Vec::new();
    let mut warnings = Vec::new();

    for result in walker.build() {
        let entry = match result {
            Ok(entry) => entry,
            Err(err) => {
                warnings.push(err.to_string());
                continue;
            }
        };
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let Ok(rel) = entry.path().strip_prefix(root) else {
            continue;
        };
        let rel = relative_string(rel);

        if let Some(name) = entry.file_name().to_str() {
            if marker_names.contains(name) {
                markers.push(rel.clone());
            }
        }

        let Some(language) = language_of(&rel) else {
            continue;
        };
        match read_source_file(root, &rel, &language) {
            Some(file) => files.push(file),
            None => warnings.push(format!("skipped unreadable or binary file: {rel}")),
        }
    }

    files.sort_by(|a, b| a.path.cmp(&b.path));
    markers.sort();
    Ok(ScanResult {
        files,
        markers,
        warnings,
    })
}

pub fn read_source_file(root: &Path, rel: &str, language: &str) -> Option<SourceFile> {
    let bytes = fs::read(root.join(rel)).ok()?;
    if bytes.contains(&0) {
        return None;
    }
    let mut lines = bytes.iter().filter(|&&b| b == b'\n').count() as i64;
    if bytes.last().is_some_and(|&b| b != b'\n') {
        lines += 1;
    }
    Some(SourceFile {
        path: rel.to_string(),
        language: language.to_string(),
        size: bytes.len() as u64,
        hash: format!("{:x}", Sha256::digest(&bytes)),
        lines,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{default_config, detect_language};
    /// A uniquely named, private directory that is removed on drop, even if a test panics.
    struct TempProject(tempfile::TempDir);

    impl TempProject {
        fn new() -> Self {
            TempProject(tempfile::Builder::new().prefix("ccg-scan-").tempdir().unwrap())
        }
        fn path(&self) -> &Path {
            self.0.path()
        }
        fn write(&self, rel: &str, content: &str) {
            let path = self.path().join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, content).unwrap();
        }
    }

    fn language_of(path: &str) -> Option<String> {
        detect_language(path).map(String::from)
    }

    fn scan(project: &TempProject, config: &Config, markers: &[&str]) -> ScanResult {
        let marker_names = markers.iter().map(|m| m.to_string()).collect();
        scan_project(project.path(), config, &language_of, &marker_names).unwrap()
    }

    fn paths(result: &ScanResult) -> Vec<&str> {
        result.files.iter().map(|f| f.path.as_str()).collect()
    }

    #[test]
    fn source_directories_named_db_and_data_are_scanned_but_dependency_directories_are_not() {
        let p = TempProject::new();
        p.write("src/db/connection.ts", "export const a = 1;\n");
        p.write("pkg/data/loader.py", "x = 1\n");
        p.write("node_modules/leftpad/index.js", "module.exports = 1;\n");
        p.write("web/node_modules/deep/index.js", "module.exports = 2;\n");
        p.write("target/debug/build.rs", "fn main() {}\n");
        p.write("notes.txt", "not source\n");

        let result = scan(&p, &default_config(p.path()), &[]);
        assert_eq!(paths(&result), vec!["pkg/data/loader.py", "src/db/connection.ts"]);
    }

    #[test]
    fn gitignore_is_respected_and_can_be_switched_off() {
        let p = TempProject::new();
        p.write(".gitignore", "generated/\n");
        p.write("generated/api.ts", "export {};\n");
        p.write("src/app.ts", "export {};\n");

        let mut config = default_config(p.path());
        assert_eq!(paths(&scan(&p, &config, &[])), vec!["src/app.ts"]);

        config.respect_gitignore = false;
        assert_eq!(paths(&scan(&p, &config, &[])), vec!["generated/api.ts", "src/app.ts"]);
    }

    #[test]
    fn user_patterns_use_gitignore_syntax_including_negation() {
        let p = TempProject::new();
        p.write("a.min.js", "1\n");
        p.write("keep.min.js", "2\n");
        p.write("legacy/old.py", "x = 1\n");
        p.write("src/new.py", "x = 1\n");

        let mut config = default_config(p.path());
        config.ignore_patterns.push("legacy/".to_string());
        config.ignore_patterns.push("!keep.min.js".to_string());

        assert_eq!(paths(&scan(&p, &config, &[])), vec!["keep.min.js", "src/new.py"]);
    }

    #[test]
    fn marker_files_are_collected_even_though_they_are_not_source_files() {
        let p = TempProject::new();
        p.write("rust-cli/Cargo.toml", "[package]\n");
        p.write("rust-cli/main.rs", "fn main() {}\n");
        p.write("web/package.json", "{}\n");
        p.write("node_modules/x/package.json", "{}\n");

        let result = scan(&p, &default_config(p.path()), &["Cargo.toml", "package.json"]);
        assert_eq!(result.markers, vec!["rust-cli/Cargo.toml", "web/package.json"]);
        assert_eq!(paths(&result), vec!["rust-cli/main.rs"]);
    }

    #[test]
    fn lines_size_and_hash_are_recorded_and_binary_files_are_skipped() {
        let p = TempProject::new();
        p.write("a.py", "one\ntwo\nthree");
        p.write("b.py", "one\ntwo\n");
        p.write("empty.py", "");
        fs::write(p.path().join("bin.py"), b"\x00\x01\x02").unwrap();

        let result = scan(&p, &default_config(p.path()), &[]);
        let by_path = |name: &str| result.files.iter().find(|f| f.path == name).unwrap();
        assert_eq!(by_path("a.py").lines, 3, "last line without newline still counts");
        assert_eq!(by_path("b.py").lines, 2);
        assert_eq!(by_path("empty.py").lines, 0);
        assert_eq!(by_path("a.py").size, 13);
        assert_eq!(by_path("a.py").hash.len(), 64);
        assert!(!paths(&result).contains(&"bin.py"));
        assert!(result.warnings.iter().any(|w| w.contains("bin.py")));
    }
}
