//! Running the SCIP indexers.
//!
//! The heavy lifting (name resolution, type inference) is done by the industry-standard
//! tool for each language; this module finds where to run it, runs it, and reports what
//! happened, including when the tool is not installed.

use crate::config::{self, Config, IndexerSpec};
use crate::scanner::ScanResult;
use protobuf::Message;
use scip::types::Index;
use serde::Serialize;
use std::collections::{BTreeSet, HashSet};
use std::ffi::OsString;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

/// The indexers that ship with the tool. Each is the reference SCIP indexer for its
/// language: rust-analyzer (Rust), scip-typescript (TypeScript compiler API, covers
/// JavaScript too) and scip-python (built on Pyright).
pub fn builtin_indexers() -> Vec<IndexerSpec> {
    vec![
        IndexerSpec {
            name: "rust-analyzer".into(),
            enabled: true,
            languages: strings(&["rust"]),
            extensions: vec![],
            markers: strings(&["Cargo.toml"]),
            command: strings(&["rust-analyzer", "scip", ".", "--output", "{output}"]),
            version_args: strings(&["--version"]),
            install_hint: "rustup component add rust-analyzer".into(),
        },
        IndexerSpec {
            name: "scip-typescript".into(),
            enabled: true,
            languages: strings(&["typescript", "javascript"]),
            extensions: vec![],
            markers: strings(&["tsconfig.json", "jsconfig.json", "package.json"]),
            // `{tsconfig}` is the project's own tsconfig.json/jsconfig.json, or a generated one
            // kept under .codebase-context. (scip-typescript's --infer-tsconfig would write a
            // tsconfig.json into the project and leave it there.)
            command: strings(&[
                "scip-typescript",
                "index",
                "{tsconfig}",
                "--no-progress-bar",
                "--output",
                "{output}",
            ]),
            version_args: strings(&["--version"]),
            install_hint: "npm install -g @sourcegraph/scip-typescript".into(),
        },
        IndexerSpec {
            name: "scip-python".into(),
            enabled: true,
            languages: strings(&["python"]),
            extensions: vec![],
            markers: strings(&["pyproject.toml", "setup.py", "setup.cfg", "requirements.txt"]),
            // scip-python crashes when it cannot work out a project version (no package
            // metadata and not a git checkout), so always give it one. Node ids leave the
            // version out, so the value does not matter.
            command: strings(&[
                "scip-python",
                "index",
                "--project-name",
                "{project_name}",
                "--project-version",
                "0.0.0",
                "--quiet",
                "--output",
                "{output}",
            ]),
            version_args: strings(&["--version"]),
            install_hint: "npm install -g @sourcegraph/scip-python".into(),
        },
    ]
}

/// Built-in indexers, with entries from `config.json` replacing a built-in of the same
/// name or adding a new one. Disabled indexers are dropped.
pub fn effective_specs(config: &Config) -> Vec<IndexerSpec> {
    let mut specs = builtin_indexers();
    for custom in &config.indexers {
        match specs.iter_mut().find(|s| s.name == custom.name) {
            Some(slot) => *slot = custom.clone(),
            None => specs.push(custom.clone()),
        }
    }
    specs.retain(|s| s.enabled);
    specs
}

/// Language of a file: built-in detection first, then extensions declared by custom indexers.
pub fn language_of(path: &str, specs: &[IndexerSpec]) -> Option<String> {
    if let Some(language) = config::detect_language(path) {
        return Some(language.to_string());
    }
    let ext = Path::new(path).extension()?.to_str()?.to_ascii_lowercase();
    specs
        .iter()
        .find(|s| {
            s.extensions
                .iter()
                .any(|e| e.trim_start_matches('.').eq_ignore_ascii_case(&ext))
        })
        .and_then(|s| s.languages.first().cloned())
}

pub fn marker_names(specs: &[IndexerSpec]) -> HashSet<String> {
    specs.iter().flat_map(|s| s.markers.iter().cloned()).collect()
}

/// Directories (relative to the project root, "" for the root itself) that contain one of
/// `markers`, keeping only the top-most ones: a `Cargo.toml` inside a directory that
/// already has one is a workspace member and is covered by the outer run.
pub fn top_most_roots(marker_paths: &[String], markers: &[String]) -> Vec<String> {
    let dirs: BTreeSet<String> = marker_paths
        .iter()
        .filter_map(|path| {
            let (dir, name) = path.rsplit_once('/').unwrap_or(("", path.as_str()));
            markers.iter().any(|m| m == name).then(|| dir.to_string())
        })
        .collect();
    // Sorted order puts every directory after its ancestors.
    let mut kept: Vec<String> = Vec::new();
    for dir in dirs {
        let nested = kept
            .iter()
            .any(|k| k.is_empty() || dir == *k || dir.starts_with(&format!("{k}/")));
        if !nested {
            kept.push(dir);
        }
    }
    kept
}

#[derive(Debug, Clone)]
pub struct Job {
    pub spec: IndexerSpec,
    /// Where to run, relative to the project root ("" = the project root).
    pub root: String,
    pub output: PathBuf,
    pub log: PathBuf,
    /// Directory names to leave out of a generated tsconfig (from the ignore patterns).
    pub exclude_dirs: Vec<String>,
}

/// Plain directory names from gitignore-style patterns (`node_modules/`, `/dist`), for
/// tools that take their own exclude lists. Patterns with wildcards or sub-paths are skipped.
pub fn plain_dir_names(patterns: &[String]) -> Vec<String> {
    patterns
        .iter()
        .filter_map(|p| {
            let name = p.trim_start_matches('/').trim_end_matches('/');
            let plain = !name.is_empty()
                && !name.starts_with('!')
                && !name.contains(['*', '?', '[', '/']);
            (plain && p.ends_with('/')).then(|| name.to_string())
        })
        .collect()
}

/// Decides which indexers to run where: one run per project root, for every indexer whose
/// languages occur in the project. Without any marker file the project root is used.
pub fn plan_jobs(
    scan: &ScanResult,
    specs: &[IndexerSpec],
    out_dir: &Path,
    exclude_dirs: &[String],
) -> Vec<Job> {
    let mut jobs = Vec::new();
    for spec in specs {
        let files: Vec<&str> = scan
            .files
            .iter()
            .filter(|f| spec.languages.contains(&f.language))
            .map(|f| f.path.as_str())
            .collect();
        if files.is_empty() {
            continue;
        }
        let mut roots: Vec<String> = top_most_roots(&scan.markers, &spec.markers)
            .into_iter()
            .filter(|root| {
                root.is_empty() || files.iter().any(|f| f.starts_with(&format!("{root}/")))
            })
            .collect();
        if roots.is_empty() {
            roots.push(String::new());
        }
        for (n, root) in roots.into_iter().enumerate() {
            let stem = if n == 0 {
                spec.name.clone()
            } else {
                format!("{}-{}", spec.name, n + 1)
            };
            jobs.push(Job {
                spec: spec.clone(),
                root,
                output: out_dir.join(format!("{stem}.scip")),
                log: out_dir.join(format!("{stem}.log")),
                exclude_dirs: exclude_dirs.to_vec(),
            });
        }
    }
    jobs
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RunStatus {
    Ok,
    /// The indexer is not installed (or does not start).
    Missing,
    Failed,
    /// A prebuilt index passed with `--scip`.
    External,
}

impl RunStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            RunStatus::Ok => "ok",
            RunStatus::Missing => "missing",
            RunStatus::Failed => "failed",
            RunStatus::External => "external",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct RunReport {
    pub indexer: String,
    pub root: String,
    pub status: RunStatus,
    pub message: String,
    /// Tool name and version as reported by the tool itself.
    pub tool: String,
    pub documents: usize,
    pub occurrences: usize,
    pub duration_ms: u64,
    pub output: Option<String>,
}

impl RunReport {
    fn new(job_name: &str, root: &str, status: RunStatus, message: String) -> Self {
        RunReport {
            indexer: job_name.to_string(),
            root: root.to_string(),
            status,
            message,
            tool: String::new(),
            documents: 0,
            occurrences: 0,
            duration_ms: 0,
            output: None,
        }
    }
}

/// Where these tools usually live. A process started from a GUI (apps launched from Finder
/// get a bare `PATH`) does not see `~/.cargo/bin`, Homebrew or npm's global bin otherwise.
fn common_tool_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        for sub in [".cargo/bin", ".local/bin", ".volta/bin", ".npm-global/bin"] {
            dirs.push(home.join(sub));
        }
    }
    dirs.extend(["/opt/homebrew/bin", "/usr/local/bin"].map(PathBuf::from));
    dirs
}

pub fn find_program(name: &str) -> Option<PathBuf> {
    find_program_in(name, std::env::var_os("PATH"), &common_tool_dirs())
}

fn find_program_in(name: &str, path_var: Option<OsString>, fallback: &[PathBuf]) -> Option<PathBuf> {
    let candidate = Path::new(name);
    if candidate.components().count() > 1 {
        return candidate.is_file().then(|| candidate.to_path_buf());
    }
    let mut dirs: Vec<PathBuf> = path_var
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    dirs.extend(fallback.iter().cloned());
    dirs.into_iter().map(|d| d.join(name)).find(|p| is_executable(p))
}

/// `PATH` for an indexer process: the inherited one, then the directory the program was
/// found in (a global npm package's `node` lives next to it) and the usual tool directories.
fn child_path(program: &Path) -> OsString {
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    dirs.extend(program.parent().map(Path::to_path_buf));
    dirs.extend(common_tool_dirs());
    let mut seen = HashSet::new();
    dirs.retain(|d| seen.insert(d.clone()));
    std::env::join_paths(dirs).unwrap_or_default()
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// Runs `program args...` and returns the first line it prints, or the reason it failed.
pub fn probe_version(program: &Path, args: &[String]) -> Result<String, String> {
    let output = Command::new(program)
        .args(args)
        .env("PATH", child_path(program))
        .stdin(Stdio::null())
        .output()
        .map_err(|e| e.to_string())?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let first = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    if output.status.success() {
        Ok(first.to_string())
    } else {
        Err(first.to_string())
    }
}

pub fn sanitize_project_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '-' })
        .collect();
    if cleaned.trim_matches('-').is_empty() {
        "project".to_string()
    } else {
        cleaned
    }
}

fn expand(arg: &str, output: &Path, project_name: &str, tsconfig: Option<&Path>) -> String {
    arg.replace("{output}", &output.to_string_lossy())
        .replace("{project_name}", &sanitize_project_name(project_name))
        .replace(
            "{tsconfig}",
            &tsconfig.map(|p| p.to_string_lossy().to_string()).unwrap_or_default(),
        )
}

/// The project's own `tsconfig.json` (or `jsconfig.json`) when it has one; otherwise a
/// generated config that covers both TypeScript and JavaScript. The generated file lives next
/// to the indexer log, so nothing is ever written into the project.
fn prepare_tsconfig(root: &Path, job: &Job) -> Result<PathBuf, String> {
    for name in ["tsconfig.json", "jsconfig.json"] {
        let own = root.join(name);
        if own.is_file() {
            return Ok(own);
        }
    }
    let base = root.to_string_lossy().replace('\\', "/");
    let config = serde_json::json!({
        "compilerOptions": { "allowJs": true, "jsx": "preserve", "skipLibCheck": true, "noEmit": true },
        "include": [format!("{base}/**/*")],
        "exclude": job.exclude_dirs.iter().map(|d| format!("{base}/**/{d}")).collect::<Vec<_>>(),
    });
    let path = job.log.with_extension("tsconfig.json");
    fs::write(&path, serde_json::to_string_pretty(&config).unwrap_or_default())
        .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    Ok(path)
}

/// A backtrace frame such as `   6: std::rt::lang_start` or `at foo (file.ts:1:2)`.
fn is_stack_frame(line: &str) -> bool {
    let line = line.trim_start();
    if line.starts_with("at ") || line.starts_with("Stack backtrace") {
        return true;
    }
    line.split_once(':').is_some_and(|(number, rest)| {
        !number.is_empty()
            && number.chars().all(|c| c.is_ascii_digit())
            && rest.starts_with(' ')
            && !rest.trim_start().starts_with(|c: char| c.is_ascii_digit())
    })
}

/// What went wrong, from an indexer's log. Tools print the actual error first and a long
/// backtrace after it, so frames are dropped and the last error-looking lines are kept.
pub fn summarize_failure(log: &str) -> String {
    let lines: Vec<&str> = log
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !is_stack_frame(l))
        .collect();
    let looks_like_error = |line: &&str| {
        let lower = line.to_ascii_lowercase();
        ["error", "fatal", "failed", "cannot", "can't", "no projects", "not found", "panicked", "exception", "invalid"]
            .iter()
            .any(|word| lower.contains(word))
    };
    let errors: Vec<&str> = lines.iter().copied().filter(looks_like_error).collect();
    let chosen: Vec<&str> = if errors.is_empty() {
        lines[lines.len().saturating_sub(3)..].to_vec()
    } else {
        errors[errors.len().saturating_sub(2)..].to_vec()
    };
    let summary = chosen.join(" | ");
    if summary.chars().count() > 400 {
        format!("{}…", summary.chars().take(400).collect::<String>())
    } else {
        summary
    }
}

pub fn load_index(path: &Path) -> Result<Index, String> {
    let bytes = fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    Index::parse_from_bytes(&bytes)
        .map_err(|e| format!("{} is not a valid SCIP index: {e}", path.display()))
}

pub fn count_occurrences(index: &Index) -> usize {
    index.documents.iter().map(|d| d.occurrences.len()).sum()
}

/// Runs one indexer. Never panics and never fails the whole run: the outcome, including
/// "not installed", comes back as a report so it can be shown and stored.
pub fn run_job(project_root: &Path, job: &Job, project_name: &str, timeout: Duration) -> RunReport {
    let name = job.spec.name.as_str();
    let report = |status, message: String| RunReport::new(name, &job.root, status, message);
    let hint = if job.spec.install_hint.is_empty() {
        String::new()
    } else {
        format!(" Install: {}", job.spec.install_hint)
    };

    let Some(program) = job.spec.command.first() else {
        return report(RunStatus::Failed, "no command configured".to_string());
    };
    let Some(program_path) = find_program(program) else {
        return report(RunStatus::Missing, format!("`{program}` is not on PATH.{hint}"));
    };
    let mut tool = String::new();
    if !job.spec.version_args.is_empty() {
        match probe_version(&program_path, &job.spec.version_args) {
            Ok(version) => tool = version,
            Err(why) => {
                return report(
                    RunStatus::Missing,
                    format!("`{program} {}` failed ({why}).{hint}", job.spec.version_args.join(" ")),
                );
            }
        }
    }

    if let Some(dir) = job.output.parent() {
        if let Err(e) = fs::create_dir_all(dir) {
            return report(RunStatus::Failed, format!("cannot create {}: {e}", dir.display()));
        }
    }
    let _ = fs::remove_file(&job.output);
    let output = fs::canonicalize(job.output.parent().unwrap_or(Path::new(".")))
        .map(|dir| dir.join(job.output.file_name().unwrap_or_default()))
        .unwrap_or_else(|_| job.output.clone());

    let log = match File::create(&job.log).and_then(|f| f.try_clone().map(|g| (f, g))) {
        Ok(pair) => pair,
        Err(e) => return report(RunStatus::Failed, format!("cannot write {}: {e}", job.log.display())),
    };
    let run_dir = project_root.join(&job.root);
    let tsconfig = if job.spec.command.iter().any(|a| a.contains("{tsconfig}")) {
        let root = fs::canonicalize(&run_dir).unwrap_or_else(|_| run_dir.clone());
        match prepare_tsconfig(&root, job) {
            Ok(path) => Some(path),
            Err(why) => return report(RunStatus::Failed, why),
        }
    } else {
        None
    };
    let mut child = match Command::new(&program_path)
        .args(
            job.spec.command[1..]
                .iter()
                .map(|a| expand(a, &output, project_name, tsconfig.as_deref())),
        )
        .current_dir(&run_dir)
        .env("PATH", child_path(&program_path))
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.0))
        .stderr(Stdio::from(log.1))
        .spawn()
    {
        Ok(child) => child,
        Err(e) => return report(RunStatus::Failed, format!("cannot start `{program}`: {e}")),
    };

    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if started.elapsed() > timeout => {
                let _ = child.kill();
                let _ = child.wait();
                let mut r = report(
                    RunStatus::Failed,
                    format!("timed out after {}s (raise indexer_timeout_secs in config.json)", timeout.as_secs()),
                );
                r.tool = tool;
                r.duration_ms = started.elapsed().as_millis() as u64;
                return r;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => break Err(e),
        }
    };
    let duration_ms = started.elapsed().as_millis() as u64;

    let cause = || {
        let log = fs::read_to_string(&job.log).unwrap_or_default();
        let shown = job.log.strip_prefix(project_root).unwrap_or(&job.log);
        format!("{} (log: {})", summarize_failure(&log), shown.display())
    };
    let (status, message) = match status {
        Err(e) => (RunStatus::Failed, format!("lost track of the process: {e}")),
        Ok(s) if !s.success() => (
            RunStatus::Failed,
            format!(
                "{}: {}",
                s.code().map_or_else(|| s.to_string(), |c| format!("exit code {c}")),
                cause()
            ),
        ),
        Ok(_) if !output.exists() => (
            RunStatus::Failed,
            format!("finished but wrote no index: {}", cause()),
        ),
        Ok(_) => (RunStatus::Ok, String::new()),
    };
    let mut r = report(status, message);
    r.tool = tool;
    r.duration_ms = duration_ms;
    r.output = (status == RunStatus::Ok).then(|| output.to_string_lossy().to_string());
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scanner::SourceFile;

    fn s(items: &[&str]) -> Vec<String> {
        strings(items)
    }

    fn file(path: &str, language: &str) -> SourceFile {
        SourceFile {
            path: path.to_string(),
            language: language.to_string(),
            size: 1,
            hash: "h".into(),
            lines: 1,
        }
    }

    fn scan(files: &[(&str, &str)], markers: &[&str]) -> ScanResult {
        ScanResult {
            files: files.iter().map(|(p, l)| file(p, l)).collect(),
            markers: s(markers),
            warnings: vec![],
        }
    }

    #[test]
    fn only_the_top_most_project_roots_are_kept() {
        let markers = s(&["Cargo.toml"]);
        let roots = |paths: &[&str]| top_most_roots(&s(paths), &markers);
        assert_eq!(roots(&["rust-cli/Cargo.toml"]), vec!["rust-cli"]);
        assert_eq!(roots(&["Cargo.toml", "crates/a/Cargo.toml"]), vec![""]);
        assert_eq!(
            roots(&["a/Cargo.toml", "a/b/Cargo.toml", "a-b/Cargo.toml", "z/Cargo.toml"]),
            vec!["a", "a-b", "z"],
            "a-b is a sibling of a, not a child"
        );
        assert!(roots(&["package.json"]).is_empty(), "other manifests are not markers here");
    }

    #[test]
    fn jobs_are_planned_per_language_and_per_root() {
        let specs = builtin_indexers();
        let scan = scan(
            &[
                ("rust-cli/main.rs", "rust"),
                ("web/src/a.ts", "typescript"),
                ("tools/x.py", "python"),
            ],
            &["rust-cli/Cargo.toml", "web/package.json", "web/tsconfig.json"],
        );
        let jobs = plan_jobs(&scan, &specs, Path::new("/out"), &[]);
        let plan: Vec<(&str, &str)> = jobs.iter().map(|j| (j.spec.name.as_str(), j.root.as_str())).collect();
        assert_eq!(
            plan,
            vec![
                ("rust-analyzer", "rust-cli"),
                ("scip-typescript", "web"),
                ("scip-python", ""), // no Python marker anywhere: run at the project root
            ]
        );
        assert_eq!(jobs[0].output, Path::new("/out/rust-analyzer.scip"));
    }

    #[test]
    fn indexers_for_languages_that_are_not_present_are_not_run() {
        let scan = scan(&[("a.py", "python")], &[]);
        let jobs = plan_jobs(&scan, &builtin_indexers(), Path::new("/out"), &[]);
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].spec.name, "scip-python");
    }

    #[test]
    fn several_independent_roots_get_separate_outputs() {
        let scan = scan(
            &[("a/lib.rs", "rust"), ("b/lib.rs", "rust")],
            &["a/Cargo.toml", "b/Cargo.toml"],
        );
        let jobs = plan_jobs(&scan, &builtin_indexers(), Path::new("/out"), &[]);
        let outputs: Vec<_> = jobs.iter().map(|j| j.output.file_name().unwrap().to_string_lossy().to_string()).collect();
        assert_eq!(outputs, vec!["rust-analyzer.scip", "rust-analyzer-2.scip"]);
    }

    #[test]
    fn config_can_replace_disable_and_add_indexers() {
        let mut config = crate::config::default_config(Path::new("."));
        assert_eq!(effective_specs(&config).len(), 3);

        let mut replacement = builtin_indexers().remove(0);
        replacement.command = s(&["ra-custom", "{output}"]);
        let mut disabled = builtin_indexers().remove(1);
        disabled.enabled = false;
        let added = IndexerSpec {
            name: "scip-go".into(),
            enabled: true,
            languages: s(&["go"]),
            extensions: s(&["go"]),
            markers: s(&["go.mod"]),
            command: s(&["scip-go", "--output", "{output}"]),
            version_args: vec![],
            install_hint: String::new(),
        };
        config.indexers = vec![replacement, disabled, added];

        let specs = effective_specs(&config);
        let names: Vec<&str> = specs.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["rust-analyzer", "scip-python", "scip-go"]);
        assert_eq!(specs[0].command[0], "ra-custom");
        assert_eq!(language_of("cmd/main.go", &specs).as_deref(), Some("go"));
        assert_eq!(language_of("a.rs", &specs).as_deref(), Some("rust"));
        assert_eq!(language_of("a.txt", &specs), None);
    }

    #[test]
    fn placeholders_and_project_names_are_expanded_safely() {
        let out = Path::new("/o/x.scip");
        assert_eq!(expand("--output={output}", out, "p", None), "--output=/o/x.scip");
        assert_eq!(expand("{project_name}", out, "My Project (v2)", None), "My-Project--v2-");
        assert_eq!(expand("{tsconfig}", out, "p", Some(Path::new("/c/tsconfig.json"))), "/c/tsconfig.json");
        assert_eq!(sanitize_project_name(""), "project");
        assert_eq!(sanitize_project_name("***"), "project");
    }

    #[cfg(unix)]
    #[test]
    fn programs_are_found_on_path_and_then_in_the_fallback_directories() {
        use std::os::unix::fs::PermissionsExt;
        let base = tempfile::tempdir().unwrap();
        let (on_path, fallback) = (base.path().join("a"), base.path().join("b"));
        fs::create_dir_all(&on_path).unwrap();
        fs::create_dir_all(&fallback).unwrap();
        for (dir, name, mode) in [(&on_path, "tool-a", 0o755), (&fallback, "tool-b", 0o755), (&fallback, "not-exec", 0o644)] {
            fs::write(dir.join(name), "#!/bin/sh\n").unwrap();
            fs::set_permissions(dir.join(name), fs::Permissions::from_mode(mode)).unwrap();
        }

        let path_var = Some(on_path.clone().into_os_string());
        assert_eq!(find_program_in("tool-a", path_var.clone(), &[]), Some(on_path.join("tool-a")));
        assert_eq!(find_program_in("tool-b", path_var.clone(), &[]), None);
        assert_eq!(
            find_program_in("tool-b", path_var.clone(), &[fallback.clone()]),
            Some(fallback.join("tool-b")),
            "a GUI-launched process has a bare PATH, so the usual tool directories are searched too"
        );
        assert_eq!(find_program_in("not-exec", path_var, &[fallback]), None, "must be executable");
        assert_eq!(find_program_in("/definitely/not/here", None, &[]), None);
    }

    #[test]
    fn the_indexer_process_gets_the_directory_its_program_lives_in_on_its_path() {
        let path = child_path(Path::new("/opt/nvm/bin/scip-typescript"));
        let dirs: Vec<PathBuf> = std::env::split_paths(&path).collect();
        assert_eq!(dirs.iter().filter(|d| *d == Path::new("/opt/nvm/bin")).count(), 1);
        let unique: HashSet<&PathBuf> = dirs.iter().collect();
        assert_eq!(unique.len(), dirs.len(), "no duplicate entries");
    }

    #[test]
    fn exclude_lists_come_from_plain_directory_ignore_patterns() {
        let patterns = s(&["node_modules/", "/dist/", "*.min.js", "docs/api/", "!keep/", "target", "build/"]);
        assert_eq!(plain_dir_names(&patterns), vec!["node_modules", "dist", "build"]);
    }

    #[test]
    fn stack_frames_are_recognised_but_log_lines_with_timestamps_are_not() {
        assert!(is_stack_frame("   6: std::rt::lang_start::<()>"));
        assert!(is_stack_frame("at normalizeNameOrVersion (/x/ScipSymbol.ts:23:11)"));
        assert!(is_stack_frame("Stack backtrace:"));
        assert!(!is_stack_frame("10:42:01 ERROR could not load workspace"));
        assert!(!is_stack_frame("Error: no projects"));
    }

    #[test]
    fn the_cause_is_extracted_from_real_indexer_logs_not_the_backtrace() {
        // rust-analyzer, run in a directory without Cargo.toml: cause first, frames after.
        let rust = "Generating SCIP start...\nError: no projects\n\nStack backtrace:\n   0: <anyhow::Error>::msg::<&str>\n   1: <project_model::ProjectManifest>::discover_single\n  10: __libc_start_main_impl\n             at ./csu/../csu/libc-start.c:360:3\n  11: <unknown>\n";
        assert_eq!(summarize_failure(rust), "Error: no projects");

        // scip-python crash: a harmless git warning first, then the crash, then a JS stack.
        let python = "fatal: not a git repository (or any of the parent directories): .git\nWarning: Could not find package information for: \n\n\nExperienced Fatal Error While Indexing:\nPlease create an issue at github.com/sourcegraph/scip-python: {\n  currentFilepath: '/p/main.py',\n  error: TypeError: Cannot read properties of undefined (reading 'indexOf')\n      at normalizeNameOrVersion (/x/ScipSymbol.ts:23:11)\n      at Function.static (/x/ScipSymbol.ts:11:19)\n";
        assert_eq!(
            summarize_failure(python),
            "Experienced Fatal Error While Indexing: | error: TypeError: Cannot read properties of undefined (reading 'indexOf')"
        );

        // No error-looking line at all: fall back to the last lines.
        assert_eq!(summarize_failure("one\ntwo\nthree\nfour\n"), "two | three | four");
        assert_eq!(summarize_failure(""), "");
    }

    fn job_for_tsconfig(dir: &Path) -> Job {
        Job {
            spec: builtin_indexers().remove(1),
            root: String::new(),
            output: dir.join(".codebase-context/scip/scip-typescript.scip"),
            log: dir.join(".codebase-context/scip/scip-typescript.log"),
            exclude_dirs: s(&["node_modules", "dist"]),
        }
    }

    #[test]
    fn typescript_indexing_uses_the_projects_own_tsconfig_when_there_is_one() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        fs::write(dir.join("tsconfig.json"), "{}").unwrap();
        let job = job_for_tsconfig(dir);
        assert_eq!(prepare_tsconfig(dir, &job).unwrap(), dir.join("tsconfig.json"));

        fs::remove_file(dir.join("tsconfig.json")).unwrap();
        fs::write(dir.join("jsconfig.json"), "{}").unwrap();
        assert_eq!(prepare_tsconfig(dir, &job).unwrap(), dir.join("jsconfig.json"));
    }

    #[test]
    fn without_a_tsconfig_one_is_generated_outside_the_project_and_covers_javascript() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        fs::create_dir_all(dir.join(".codebase-context/scip")).unwrap();
        let job = job_for_tsconfig(dir);

        let path = prepare_tsconfig(dir, &job).unwrap();
        assert_eq!(path, dir.join(".codebase-context/scip/scip-typescript.tsconfig.json"));
        assert!(!dir.join("tsconfig.json").exists(), "nothing is written into the project");

        let generated: serde_json::Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(generated["compilerOptions"]["allowJs"], true);
        let base = dir.to_string_lossy().replace('\\', "/");
        assert_eq!(generated["include"][0], format!("{base}/**/*"));
        assert_eq!(generated["exclude"][0], format!("{base}/**/node_modules"));
        assert_eq!(generated["exclude"][1], format!("{base}/**/dist"));
    }

    #[test]
    fn the_python_indexer_always_gets_a_project_version() {
        let python = builtin_indexers().remove(2);
        assert_eq!(python.name, "scip-python");
        let pos = python.command.iter().position(|a| a == "--project-version").expect("flag present");
        assert_eq!(python.command[pos + 1], "0.0.0");
    }

    #[cfg(unix)]
    mod running {
        use super::*;
        /// A uniquely named, private directory that is removed on drop, even if a test panics.
        fn workdir() -> tempfile::TempDir {
            tempfile::Builder::new().prefix("ccg-run-").tempdir().unwrap()
        }

        fn job(dir: &Path, command: &[&str]) -> Job {
            Job {
                spec: IndexerSpec {
                    name: "fake".into(),
                    enabled: true,
                    languages: s(&["rust"]),
                    extensions: vec![],
                    markers: vec![],
                    command: s(command),
                    version_args: vec![],
                    install_hint: "install fake".into(),
                },
                root: String::new(),
                output: dir.join("out").join("fake.scip"),
                log: dir.join("out").join("fake.log"),
                exclude_dirs: vec![],
            }
        }

        #[test]
        fn a_successful_run_reports_ok_and_leaves_the_index() {
            let tmp = workdir();
            let dir = tmp.path();
            let r = run_job(dir, &job(dir, &["sh", "-c", "echo hi; : > \"$0\"", "{output}"]), "p", Duration::from_secs(10));
            assert_eq!(r.status, RunStatus::Ok, "{}", r.message);
            let index = load_index(Path::new(r.output.as_deref().unwrap())).expect("empty file is an empty index");
            assert!(index.documents.is_empty());
        }

        #[test]
        fn a_failing_run_reports_the_tail_of_the_indexer_log() {
            let tmp = workdir();
            let dir = tmp.path();
            let r = run_job(dir, &job(dir, &["sh", "-c", "echo 'cannot find Cargo.toml' >&2; exit 3"]), "p", Duration::from_secs(10));
            assert_eq!(r.status, RunStatus::Failed);
            assert!(r.message.contains("cannot find Cargo.toml"), "{}", r.message);
            assert!(r.output.is_none());
        }

        #[test]
        fn exiting_zero_without_writing_an_index_is_a_failure() {
            let tmp = workdir();
            let dir = tmp.path();
            let r = run_job(dir, &job(dir, &["sh", "-c", "true"]), "p", Duration::from_secs(10));
            assert_eq!(r.status, RunStatus::Failed);
            assert!(r.message.contains("wrote no index"), "{}", r.message);
        }

        #[test]
        fn a_missing_program_is_reported_with_the_install_hint() {
            let tmp = workdir();
            let dir = tmp.path();
            let r = run_job(dir, &job(dir, &["definitely-not-installed-xyz"]), "p", Duration::from_secs(10));
            assert_eq!(r.status, RunStatus::Missing);
            assert!(r.message.contains("Install: install fake"), "{}", r.message);
        }

        #[test]
        fn a_hung_indexer_is_killed_at_the_timeout() {
            let tmp = workdir();
            let dir = tmp.path();
            let started = Instant::now();
            let r = run_job(dir, &job(dir, &["sh", "-c", "sleep 30"]), "p", Duration::from_millis(300));
            assert_eq!(r.status, RunStatus::Failed);
            assert!(r.message.contains("timed out"), "{}", r.message);
            assert!(started.elapsed() < Duration::from_secs(10));
        }

        #[test]
        fn a_tool_whose_version_probe_fails_counts_as_not_installed() {
            let tmp = workdir();
            let dir = tmp.path();
            let mut j = job(dir, &["sh", "-c", "true"]);
            j.spec.version_args = s(&["-c", "exit 1"]);
            let r = run_job(dir, &j, "p", Duration::from_secs(10));
            assert_eq!(r.status, RunStatus::Missing);
        }
    }
}
