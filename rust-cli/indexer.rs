use crate::config::{self, Config};
use crate::graph::{self, BuildOutput, IndexInput};
use crate::indexers::{self, RunReport, RunStatus};
use crate::scanner::{self, SourceFile};
use scip::types::Index;
use serde_json::json;
use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

type Failure = Box<dyn std::error::Error>;

#[derive(Default)]
pub struct IndexOptions {
    /// Prebuilt SCIP indexes to ingest in addition to the ones the indexers produce.
    pub scip_files: Vec<PathBuf>,
}

pub fn init_project(project_root: &PathBuf) -> Result<(), Failure> {
    let (_, created) = config::init_project_config(project_root)?;
    drop(crate::db::open_database(&config::database_path(project_root))?);

    println!(
        "{} {}",
        if created { "Created" } else { "Kept existing" },
        config::config_path(project_root).display()
    );
    println!("Next: `codebase-context-graph doctor` to check the indexers, then `index`.");
    Ok(())
}

/// Shows which SCIP indexers are installed, without touching the project.
pub fn doctor(project_root: &PathBuf) -> Result<(), Failure> {
    let config = if config::config_path(project_root).exists() {
        config::load_project_config(project_root)?
    } else {
        config::default_config(project_root)
    };
    let mut missing = 0;
    for spec in indexers::effective_specs(&config) {
        let program = spec.command.first().cloned().unwrap_or_default();
        let found = indexers::find_program(&program);
        let version = found
            .as_ref()
            .filter(|_| !spec.version_args.is_empty())
            .map(|p| indexers::probe_version(p, &spec.version_args));
        let languages = spec.languages.join(", ");
        match (found, version) {
            (Some(_), Some(Err(why))) => {
                missing += 1;
                println!("  {:<8} {:<16} {languages}: `{program}` does not run ({why})", "broken", spec.name);
                println!("      install: {}", spec.install_hint);
            }
            (Some(path), version) => {
                let version = version.and_then(Result::ok).unwrap_or_default();
                println!("  {:<8} {:<16} {languages}: {} {version}", "ok", spec.name, path.display());
            }
            (None, _) => {
                missing += 1;
                println!("  {:<8} {:<16} {languages}: `{program}` not found on PATH", "missing", spec.name);
                if !spec.install_hint.is_empty() {
                    println!("      install: {}", spec.install_hint);
                }
            }
        }
    }
    if missing > 0 {
        println!("\nFiles in languages whose indexer is missing end up in the graph without symbols.");
    }
    Ok(())
}

/// The directory an index was generated for, relative to the project root, taken from the
/// index metadata. Falls back to "" (paths are relative to the project root).
fn root_prefix_from_metadata(project_root: &Path, index: &Index) -> String {
    let uri = &index.metadata.project_root;
    let Some(raw) = uri.strip_prefix("file://") else {
        return String::new();
    };
    let indexed_root = std::fs::canonicalize(percent_decode(raw));
    let project_root = std::fs::canonicalize(project_root);
    match (indexed_root, project_root) {
        (Ok(indexed), Ok(project)) => indexed
            .strip_prefix(&project)
            .map(scanner::relative_string)
            .unwrap_or_default(),
        _ => String::new(),
    }
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let decoded = std::str::from_utf8(&bytes[i + 1..i + 3])
                .ok()
                .and_then(|hex| u8::from_str_radix(hex, 16).ok());
            if let Some(byte) = decoded {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn describe(report: &RunReport) -> String {
    match report.status {
        RunStatus::Ok | RunStatus::External => format!(
            "{} documents, {} occurrences, {:.1}s",
            report.documents,
            report.occurrences,
            report.duration_ms as f64 / 1000.0
        ),
        RunStatus::Missing | RunStatus::Failed => report.message.clone(),
    }
}

fn print_run(report: &RunReport) {
    let root = if report.root.is_empty() { "." } else { report.root.as_str() };
    println!(
        "  {:<16} {:<14} {:<9} {}",
        report.indexer,
        root,
        report.status.as_str(),
        describe(report)
    );
}

pub fn index_project(project_root: &PathBuf, options: &IndexOptions) -> Result<(), Failure> {
    let config = config::load_project_config(project_root)?;
    let specs = indexers::effective_specs(&config);

    let scan = scanner::scan_project(
        project_root,
        &config,
        &|path| indexers::language_of(path, &specs),
        &indexers::marker_names(&specs),
    )?;
    let mut per_language: BTreeMap<&str, usize> = BTreeMap::new();
    for file in &scan.files {
        *per_language.entry(file.language.as_str()).or_default() += 1;
    }
    println!(
        "Scanned {}: {} source files ({})",
        project_root.display(),
        scan.files.len(),
        per_language.iter().map(|(l, n)| format!("{l} {n}")).collect::<Vec<_>>().join(", ")
    );
    for warning in scan.warnings.iter().take(5) {
        eprintln!("  warning: {warning}");
    }

    // Run the indexers.
    let jobs = indexers::plan_jobs(
        &scan,
        &specs,
        &config::scip_dir(project_root),
        &indexers::plain_dir_names(&config.ignore_patterns),
    );
    let timeout = Duration::from_secs(config.indexer_timeout_secs);
    let mut reports: Vec<RunReport> = Vec::new();
    let mut loaded: Vec<(Index, String)> = Vec::new();
    if jobs.is_empty() && options.scip_files.is_empty() {
        println!("No indexer applies: no files in a supported language.");
    } else {
        println!("Indexers:");
    }
    for job in &jobs {
        let mut report = indexers::run_job(project_root, job, &config.project_name, timeout);
        if let Some(output) = report.output.clone() {
            match indexers::load_index(Path::new(&output)) {
                Ok(index) => {
                    report.documents = index.documents.len();
                    report.occurrences = indexers::count_occurrences(&index);
                    loaded.push((index, job.root.clone()));
                }
                Err(why) => {
                    report.status = RunStatus::Failed;
                    report.message = why;
                    report.output = None;
                }
            }
        }
        print_run(&report);
        let _ = std::io::stdout().flush();
        reports.push(report);
    }
    for path in &options.scip_files {
        let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let mut report = RunReport {
            indexer: name,
            root: String::new(),
            status: RunStatus::External,
            message: String::new(),
            tool: String::new(),
            documents: 0,
            occurrences: 0,
            duration_ms: 0,
            output: Some(path.to_string_lossy().to_string()),
        };
        match indexers::load_index(path) {
            Ok(index) => {
                report.documents = index.documents.len();
                report.occurrences = indexers::count_occurrences(&index);
                report.tool = graph::indexer_label(&index);
                let prefix = root_prefix_from_metadata(project_root, &index);
                report.root = prefix.clone();
                loaded.push((index, prefix));
            }
            Err(why) => {
                report.status = RunStatus::Failed;
                report.message = why;
            }
        }
        print_run(&report);
        reports.push(report);
    }

    // Build the graph.
    let output = build(project_root, &config, &specs, &scan.files, &loaded);
    write_outputs(project_root, &config, &output, &reports)?;
    print_summary(&output, &reports);

    if loaded.is_empty() && !scan.files.is_empty() {
        return Err(
            "no semantic index was produced, so the graph has files but no symbols. \
             Run `codebase-context-graph doctor` to see which indexers are missing."
                .into(),
        );
    }
    Ok(())
}

fn build(
    project_root: &Path,
    config: &Config,
    specs: &[config::IndexerSpec],
    scanned: &[SourceFile],
    loaded: &[(Index, String)],
) -> BuildOutput {
    let inputs: Vec<IndexInput> = loaded
        .iter()
        .map(|(index, prefix)| IndexInput {
            index,
            root_prefix: prefix.clone(),
        })
        .collect();
    let scanned_by_path: HashMap<&str, &SourceFile> =
        scanned.iter().map(|f| (f.path.as_str(), f)).collect();
    let ignore = scanner::build_matcher(project_root, &config.ignore_patterns).ok();

    let mut file_info = |path: &str| -> Option<SourceFile> {
        if let Some(file) = scanned_by_path.get(path) {
            return Some((*file).clone());
        }
        // A file in a language the scanner knows was either scanned or deliberately skipped
        // (ignored, hidden, gitignored). Anything else came from an indexer for another
        // language, and only the configured ignore patterns apply.
        if indexers::language_of(path, specs).is_some() {
            return None;
        }
        if ignore.as_ref().is_some_and(|m| m.matched(Path::new(path), false).is_ignore()) {
            return None;
        }
        let language = Path::new(path)
            .extension()
            .map(|e| e.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_else(|| "unknown".to_string());
        scanner::read_source_file(project_root, path, &language)
    };
    graph::build_graph(&inputs, scanned, &mut file_info)
}

fn write_outputs(
    project_root: &Path,
    config: &Config,
    output: &BuildOutput,
    reports: &[RunReport],
) -> Result<(), Failure> {
    let generated_at = chrono::Utc::now().to_rfc3339();
    let covered = output.files.iter().filter(|f| f.indexed_by.is_some()).count();

    let db = crate::db::open_database(&config::database_path(project_root))?;
    crate::db::replace_all(
        &db,
        &output.graph,
        &output.files,
        reports,
        &[
            ("generated_at", generated_at.clone()),
            ("project_name", config.project_name.clone()),
            ("schema_version", crate::db::SCHEMA_VERSION.to_string()),
        ],
    )?;
    drop(db);

    let payload = json!({
        "version": config::CONFIG_VERSION,
        "generatedAt": generated_at,
        "projectName": config.project_name,
        "coverage": { "files": output.files.len(), "covered": covered },
        "stats": output.stats,
        "indexers": reports,
        "files": output.files.iter().map(|f| json!({
            "path": f.file.path, "language": f.file.language, "size": f.file.size,
            "hash": f.file.hash, "lines": f.file.lines, "indexedBy": f.indexed_by,
        })).collect::<Vec<_>>(),
        "nodes": output.graph.nodes,
        "edges": output.graph.edges,
    });
    std::fs::write(
        config::graph_json_path(project_root),
        serde_json::to_string_pretty(&payload)?,
    )?;
    Ok(())
}

fn print_summary(output: &BuildOutput, reports: &[RunReport]) {
    let mut node_types: BTreeMap<&str, usize> = BTreeMap::new();
    for node in &output.graph.nodes {
        *node_types.entry(node.node_type.as_str()).or_default() += 1;
    }
    let mut edge_types: BTreeMap<&str, usize> = BTreeMap::new();
    for edge in &output.graph.edges {
        *edge_types.entry(edge.edge_type.as_str()).or_default() += 1;
    }
    let join = |m: &BTreeMap<&str, usize>| {
        m.iter().map(|(k, v)| format!("{k} {v}")).collect::<Vec<_>>().join(", ")
    };
    println!("Nodes: {}", join(&node_types));
    println!("Edges: {}", join(&edge_types));

    let uncovered: Vec<&str> = output
        .files
        .iter()
        .filter(|f| f.indexed_by.is_none())
        .map(|f| f.file.path.as_str())
        .collect();
    println!(
        "Coverage: {} of {} source files have semantic data",
        output.files.len() - uncovered.len(),
        output.files.len()
    );
    if !uncovered.is_empty() {
        let shown: Vec<&str> = uncovered.iter().copied().take(8).collect();
        let more = uncovered.len().saturating_sub(shown.len());
        println!(
            "  not covered: {}{}",
            shown.join(", "),
            if more > 0 { format!(" (+{more} more)") } else { String::new() }
        );
        let broken: Vec<&str> = reports
            .iter()
            .filter(|r| matches!(r.status, RunStatus::Missing | RunStatus::Failed))
            .map(|r| r.indexer.as_str())
            .collect();
        if !broken.is_empty() {
            println!("  indexers that did not run: {}", broken.join(", "));
        }
    }
    let stats = &output.stats;
    if stats.references_unresolved > 0 {
        println!(
            "  {} references point at project symbols no ingested file defines (ignored files or generated code)",
            stats.references_unresolved
        );
    }
    println!("Wrote .codebase-context/graph.db and graph.json");
}

pub fn watch_project(project_root: &PathBuf) -> Result<(), Failure> {
    println!("`watch` is not implemented yet: running a single index and exiting.");
    index_project(project_root, &IndexOptions::default())
}

pub fn smoke_test(project_root: &PathBuf) -> Result<(), Failure> {
    let db_path = config::database_path(project_root);

    if !db_path.exists() {
        return Err("Database not found. Run 'index' first.".into());
    }

    let db = crate::db::open_database(&db_path)?;
    let count = |sql: &str| -> Result<i64, rusqlite::Error> { db.query_row(sql, [], |r| r.get(0)) };

    let nodes = count("SELECT COUNT(*) FROM nodes")?;
    if nodes == 0 {
        return Err("Database is empty. Run 'index' first.".into());
    }
    let edges = count("SELECT COUNT(*) FROM edges")?;
    let files = count("SELECT COUNT(*) FROM file_manifest")?;
    let covered = count("SELECT COUNT(*) FROM file_manifest WHERE indexed_by IS NOT NULL")?;

    println!("Database OK: {nodes} nodes, {edges} edges, {covered}/{files} files covered");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_decoding_handles_spaces_and_leaves_plain_paths_alone() {
        assert_eq!(percent_decode("/home/u/my%20project"), "/home/u/my project");
        assert_eq!(percent_decode("/plain/path"), "/plain/path");
        assert_eq!(percent_decode("/50%"), "/50%");
        assert_eq!(percent_decode("/bad%zzescape"), "/bad%zzescape");
    }

    #[test]
    fn a_missing_or_foreign_project_root_means_paths_are_relative_to_the_project() {
        let mut index = Index::default();
        assert_eq!(root_prefix_from_metadata(Path::new("."), &index), "");
        index.metadata.mut_or_insert_default().project_root = "file:///definitely/not/here".to_string();
        assert_eq!(root_prefix_from_metadata(Path::new("."), &index), "");
    }

    #[test]
    fn a_nested_indexer_root_becomes_a_relative_prefix() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();
        let nested = base.join("rust-cli");
        std::fs::create_dir_all(&nested).unwrap();
        let mut index = Index::default();
        index.metadata.mut_or_insert_default().project_root = format!("file://{}", nested.display());
        assert_eq!(root_prefix_from_metadata(base, &index), "rust-cli");
        index.metadata.mut_or_insert_default().project_root = format!("file://{}", base.display());
        assert_eq!(root_prefix_from_metadata(base, &index), "");
    }
}
