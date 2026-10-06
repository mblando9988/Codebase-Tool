use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

pub const CONFIG_VERSION: &str = "2.0";

fn default_true() -> bool {
    true
}

fn default_timeout() -> u64 {
    1800
}

/// Gitignore-style patterns for dependency, build-output and cache directories.
///
/// Directories such as `db/`, `data/`, `logs/` or `tmp/` are deliberately *not* listed:
/// they are often real source directories. Edit `ignore_patterns` in `config.json` to
/// change the list.
pub fn default_ignore_patterns() -> Vec<String> {
    [
        ".git/",
        "node_modules/",
        "vendor/",
        "venv/",
        ".venv/",
        "site-packages/",
        "__pycache__/",
        "target/",
        "dist/",
        "build/",
        ".pytest_cache/",
        ".ruff_cache/",
        ".mypy_cache/",
        ".codebase-context/",
        "*.min.js",
        "*.map",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

/// Languages that have a built-in semantic indexer. Other languages can be added through
/// `indexers` in `config.json` (any tool that writes a SCIP index works).
pub fn detect_language(file_path: &str) -> Option<&'static str> {
    let ext = Path::new(file_path)
        .extension()?
        .to_str()?
        .to_ascii_lowercase();
    match ext.as_str() {
        "rs" => Some("rust"),
        "ts" | "tsx" | "mts" | "cts" => Some("typescript"),
        "js" | "jsx" | "mjs" | "cjs" => Some("javascript"),
        "py" | "pyi" => Some("python"),
        _ => None,
    }
}

pub fn context_dir(project_root: &Path) -> PathBuf {
    project_root.join(".codebase-context")
}

pub fn config_path(project_root: &Path) -> PathBuf {
    context_dir(project_root).join("config.json")
}

pub fn database_path(project_root: &Path) -> PathBuf {
    context_dir(project_root).join("graph.db")
}

pub fn graph_json_path(project_root: &Path) -> PathBuf {
    context_dir(project_root).join("graph.json")
}

/// Where raw SCIP indexes and indexer logs are kept.
pub fn scip_dir(project_root: &Path) -> PathBuf {
    context_dir(project_root).join("scip")
}

/// How to run one SCIP indexer. Built-in indexers live in `indexers::builtin_indexers`;
/// an entry here with the same `name` replaces the built-in one, any other name adds a new one.
///
/// In `command`, `{output}` is replaced by the absolute path the indexer must write its
/// `.scip` file to, and `{project_name}` by the project name. The indexer runs with the
/// detected project root as its working directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexerSpec {
    pub name: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Languages (as reported for files) this indexer covers.
    #[serde(default)]
    pub languages: Vec<String>,
    /// File extensions (without the dot) that map to `languages[0]`; only needed for
    /// languages without built-in detection.
    #[serde(default)]
    pub extensions: Vec<String>,
    /// File names that mark a project root for this indexer (e.g. `Cargo.toml`).
    #[serde(default)]
    pub markers: Vec<String>,
    #[serde(default)]
    pub command: Vec<String>,
    /// Arguments that make the program print its version. Empty means "don't probe".
    #[serde(default)]
    pub version_args: Vec<String>,
    #[serde(default)]
    pub install_hint: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub version: String,
    pub project_name: String,
    pub ignore_patterns: Vec<String>,
    /// Also skip whatever `.gitignore` files exclude.
    #[serde(default = "default_true")]
    pub respect_gitignore: bool,
    #[serde(default = "default_timeout")]
    pub indexer_timeout_secs: u64,
    #[serde(default)]
    pub indexers: Vec<IndexerSpec>,
}

fn default_project_name(project_root: &Path) -> String {
    fs::canonicalize(project_root)
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
        .unwrap_or_else(|| "project".to_string())
}

pub fn default_config(project_root: &Path) -> Config {
    Config {
        version: CONFIG_VERSION.to_string(),
        project_name: default_project_name(project_root),
        ignore_patterns: default_ignore_patterns(),
        respect_gitignore: true,
        indexer_timeout_secs: default_timeout(),
        indexers: vec![],
    }
}

/// Creates `config.json` with defaults only if there isn't one yet.
/// Returns the config and whether it was newly created.
pub fn init_project_config(
    project_root: &Path,
) -> Result<(Config, bool), Box<dyn std::error::Error>> {
    fs::create_dir_all(context_dir(project_root))?;
    if config_path(project_root).exists() {
        return Ok((load_project_config(project_root)?, false));
    }
    let config = default_config(project_root);
    save_project_config(project_root, &config)?;
    Ok((config, true))
}

pub fn load_project_config(project_root: &Path) -> Result<Config, Box<dyn std::error::Error>> {
    let path = config_path(project_root);
    if !path.exists() {
        return Ok(init_project_config(project_root)?.0);
    }
    let content = fs::read_to_string(&path)?;
    let config: Config = serde_json::from_str(&content)
        .map_err(|e| format!("invalid {}: {}", path.display(), e))?;
    if config.version != CONFIG_VERSION {
        eprintln!(
            "note: {} was written by an older version ({}). Its ignore list may still skip \
             directories like db/ or data/; delete the file to regenerate the defaults.",
            path.display(),
            config.version
        );
    }
    Ok(config)
}

pub fn save_project_config(
    project_root: &Path,
    config: &Config,
) -> Result<(), Box<dyn std::error::Error>> {
    let content = serde_json::to_string_pretty(config)?;
    fs::write(config_path(project_root), content)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_ignores_do_not_hide_real_source_directories() {
        let patterns = default_ignore_patterns();
        for dir in ["db/", "data/", "logs/", "tmp/", "archive/", "env/"] {
            assert!(!patterns.iter().any(|p| p == dir), "{dir} must not be ignored by default");
        }
        assert!(patterns.iter().any(|p| p == "node_modules/"));
        assert!(patterns.iter().any(|p| p == "target/"));
    }

    #[test]
    fn language_detection_covers_the_indexed_languages() {
        assert_eq!(detect_language("src/a.rs"), Some("rust"));
        assert_eq!(detect_language("a/b.TSX"), Some("typescript"));
        assert_eq!(detect_language("a.d.ts"), Some("typescript"));
        assert_eq!(detect_language("a.mjs"), Some("javascript"));
        assert_eq!(detect_language("pkg/a.py"), Some("python"));
        assert_eq!(detect_language("scripts/deploy.sh"), None, "bash has no semantic indexer");
        assert_eq!(detect_language("Makefile"), None);
    }

    #[test]
    fn a_config_written_by_the_old_version_still_loads() {
        let old = r#"{"version":"1.0","project_name":"x","languages":[],
            "ignore_patterns":["data/**"],"analysis_mode":"standard"}"#;
        let config: Config = serde_json::from_str(old).expect("old config must deserialize");
        assert_eq!(config.ignore_patterns, vec!["data/**"]);
        assert!(config.respect_gitignore);
        assert!(config.indexers.is_empty());
    }

    #[test]
    fn indexer_specs_default_to_enabled() {
        let spec: IndexerSpec = serde_json::from_str(r#"{"name":"scip-go"}"#).unwrap();
        assert!(spec.enabled);
        assert!(spec.command.is_empty());
    }
}
