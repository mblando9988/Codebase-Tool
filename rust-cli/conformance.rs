//! Conformance: every registered tool is held to the same rules, with inputs taken from real
//! indexes (built from what rust-analyzer, scip-typescript and scip-python wrote for the
//! fixtures) and from a generated project with thousands of symbols.
//!
//! Nothing here is written per tool except `cases`, which says what valid calls look like, and
//! the oracles, which recompute an answer a different way. The rules themselves (size, paging,
//! schema, determinism, honest totals, no writes) apply to whatever the registry holds, and a
//! test fails if a tool is registered without cases.

use crate::config::{self, IndexerSpec};
use crate::indexer::{self, IndexOptions};
use crate::mcp::Kind as ParamKind;
use crate::mcp::{
    self, BYTES_PER_TOKEN, DEFAULT_BUDGET_TOKENS, MAX_BUDGET_TOKENS, MIN_BUDGET_TOKENS, Server, ToolDef, WINDOW,
    file_hash, open_read_only,
};
use crate::schema;
use crate::symbols::SYMBOL_TYPES;
use crate::tools;
use protobuf::{EnumOrUnknown, Message};
use scip::types::symbol_information::Kind;
use scip::types::{Document, Index, Occurrence, SymbolInformation};
use serde_json::{Value, json};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

// ------------------------------------------------------------------------------------------
// Projects.
// ------------------------------------------------------------------------------------------

pub struct Project {
    _dir: tempfile::TempDir,
    pub root: PathBuf,
}

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).unwrap();
        }
    }
}

/// Indexes `root` from a `.scip` file only. The indexers themselves are switched off, so the
/// result does not depend on what is installed on the machine running the tests.
fn index_from_scip(root: &Path, scip: &Path) {
    let mut config = config::default_config(root);
    config.indexers = ["rust-analyzer", "scip-typescript", "scip-python"]
        .iter()
        .map(|name| IndexerSpec {
            name: name.to_string(),
            enabled: false,
            languages: vec![],
            extensions: vec![],
            markers: vec![],
            command: vec![],
            version_args: vec![],
            install_hint: String::new(),
        })
        .collect();
    fs::create_dir_all(config::context_dir(root)).unwrap();
    config::save_project_config(root, &config).unwrap();
    indexer::index_project(&root.to_path_buf(), &IndexOptions { scip_files: vec![scip.to_path_buf()] }).unwrap();
}

/// A copy of fixture project `name` (`ts`, `py` or `rust`), indexed from the `.scip` file the real
/// indexer wrote for it.
pub fn fixture_project(name: &str) -> Project {
    let dir = tempfile::Builder::new().prefix("ccg-fixture-").tempdir().unwrap();
    let root = dir.path().join(format!("fixture-{name}"));
    copy_tree(&fixtures_dir().join(name), &root);
    index_from_scip(&root, &fixtures_dir().join("scip").join(format!("{name}.scip")));
    Project { _dir: dir, root }
}

const REAL: [&str; 3] = ["ts", "py", "rust"];

fn new_server(project: &Project) -> Server {
    let mut server = Server::new(&project.root, tools::registry());
    let reply = server
        .handle_message(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18"}}))
        .unwrap();
    assert_eq!(reply["result"]["protocolVersion"], "2025-06-18");
    server
}

// ---- the generated project ----

const SYN_FILES: usize = 130;
const SYN_FUNCS: usize = 50;
const SYN_PREFIX: &str = "rust-analyzer cargo synth 0.1.0 ";
const HUB_A: &str = "hub/hub_a().";
const HUB_B: &str = "hub/hub_b().";

fn occurrence(range: &[i32], symbol: &str, roles: i32, enclosing: &[i32]) -> Occurrence {
    Occurrence {
        range: range.to_vec(),
        symbol: format!("{SYN_PREFIX}{symbol}"),
        symbol_roles: roles,
        enclosing_range: enclosing.to_vec(),
        ..Default::default()
    }
}

/// A source file and the SCIP document that describes it, kept in step line by line.
struct SynFile {
    path: String,
    text: String,
    occurrences: Vec<Occurrence>,
    symbols: Vec<SymbolInformation>,
    lines: i32,
}

impl SynFile {
    fn new(path: &str) -> SynFile {
        SynFile { path: path.to_string(), text: String::new(), occurrences: vec![], symbols: vec![], lines: 0 }
    }

    fn line(&mut self, text: &str) {
        self.text.push_str(text);
        self.text.push('\n');
        self.lines += 1;
    }

    /// `pub fn name() {`, one line per call (`<prefix><callee>();`), `}`. The definition covers all
    /// of it; each call is a reference on its own line.
    fn function(&mut self, name: &str, symbol: &str, calls: &[(&str, &str, &str)]) {
        let first = self.lines;
        self.line(&format!("pub fn {name}() {{"));
        for (prefix, callee, target) in calls {
            let at = self.lines;
            self.line(&format!("    {prefix}{callee}();"));
            let column = 4 + prefix.len() as i32;
            self.occurrences.push(occurrence(&[at, column, column + callee.len() as i32], target, 0, &[]));
        }
        self.line("}");
        let last = self.lines - 1;
        self.occurrences.push(occurrence(&[first, 7, 7 + name.len() as i32], symbol, 1, &[first, 0, last, 1]));
        self.symbols.push(SymbolInformation {
            symbol: format!("{SYN_PREFIX}{symbol}"),
            kind: EnumOrUnknown::new(Kind::Function),
            ..Default::default()
        });
    }
}

fn write_synthetic(root: &Path, files: &[SynFile], uncovered: &[(&str, &str)]) -> PathBuf {
    let mut index = Index::default();
    {
        let metadata = index.metadata.mut_or_insert_default();
        metadata.project_root = format!("file://{}", root.display());
        let tool = metadata.tool_info.mut_or_insert_default();
        tool.name = "synth-indexer".to_string();
        tool.version = "1".to_string();
    }
    for file in files {
        let path = root.join(&file.path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, &file.text).unwrap();
        index.documents.push(Document {
            relative_path: file.path.clone(),
            language: "rust".to_string(),
            occurrences: file.occurrences.clone(),
            symbols: file.symbols.clone(),
            ..Default::default()
        });
    }
    for (path, text) in uncovered {
        fs::create_dir_all(root.join(path).parent().unwrap()).unwrap();
        fs::write(root.join(path), text).unwrap();
    }
    let scip = root.parent().unwrap().join("synth.scip");
    fs::write(&scip, index.write_to_bytes().unwrap()).unwrap();
    scip
}

/// 130 modules of 50 functions: each calls the next one in its file, the same-numbered function
/// in the next module (a cycle across modules) and a shared hub. Plus the awkward cases a real
/// project throws up: ids and paths past every limit, a name with quotes and a backslash, a
/// duplicate definition, a covered file with no definitions, an uncovered file, a file of 1,000
/// short lines, and near-identical names to rank.
pub fn synthetic_project() -> Project {
    let dir = tempfile::Builder::new().prefix("ccg-synth-").tempdir().unwrap();
    let root = dir.path().join("synth");
    fs::create_dir_all(&root).unwrap();
    let mut files = Vec::new();

    for m in 0..SYN_FILES {
        let mut file = SynFile::new(&format!("src/m{m}.rs"));
        file.line("// generated module");
        let next_module = format!("m{}::", (m + 1) % SYN_FILES);
        for f in 0..SYN_FUNCS {
            let name = format!("f{f}");
            let (chain_callee, chain_symbol) = if f + 1 < SYN_FUNCS {
                (format!("f{}", f + 1), format!("m{m}/f{}().", f + 1))
            } else {
                ("hub_b".to_string(), HUB_B.to_string())
            };
            let across_symbol = format!("m{}/{name}().", (m + 1) % SYN_FILES);
            file.function(
                &name,
                &format!("m{m}/{name}()."),
                &[
                    ("", "hub_a", HUB_A),
                    ("", &chain_callee, &chain_symbol),
                    (&next_module, &name, &across_symbol),
                ],
            );
        }
        if m == 0 {
            let mut info = SymbolInformation {
                symbol: format!("{SYN_PREFIX}m0/f0()."),
                kind: EnumOrUnknown::new(Kind::Function),
                documentation: vec!["d".repeat(2000)],
                ..Default::default()
            };
            info.signature_documentation.mut_or_insert_default().text = format!("pub fn f0() {}", "s".repeat(900));
            file.symbols[0] = info;
        }
        files.push(file);
    }

    let mut hub = SynFile::new("src/hub.rs");
    hub.line("// hubs");
    hub.function("hub_a", HUB_A, &[]);
    hub.function("hub_b", HUB_B, &[]);
    files.push(hub);

    // A name far longer than an id may be, one near the limit, a name with a quote and a
    // backslash, a duplicate.
    let long_name = "l".repeat(700);
    let mid_name = "m".repeat(500);
    let mut odd = SynFile::new("src/odd.rs");
    odd.line("// odd names");
    odd.function(&long_name, &format!("odd/{long_name}()."), &[("", "hub_a", HUB_A)]);
    odd.function(&mid_name, &format!("odd/{mid_name}()."), &[("", "hub_a", HUB_A)]);
    odd.function("we\"ird\\name", "odd/`we\"ird\\name`().", &[("", "hub_a", HUB_A)]);
    odd.function("orphan", "odd/orphan().", &[]);
    files.push(odd);
    for copy in ["dup1", "dup2"] {
        let mut file = SynFile::new(&format!("src/{copy}.rs"));
        file.line("// the same symbol defined twice");
        file.function("dup_fn", "dup/dup_fn().", &[("", "hub_a", HUB_A)]);
        files.push(file);
    }

    // Names that differ only by how they contain "render".
    let mut names = SynFile::new("src/names.rs");
    names.line("// names to rank");
    for name in ["render", "renderer", "prerender", "unrender_all", "Render", "RENDER_TWICE"] {
        names.function(name, &format!("names/{name}()."), &[("", "hub_a", HUB_A)]);
    }
    files.push(names);
    let mut more = SynFile::new("src/more.rs");
    more.line("// another render");
    more.function("render", "more/render().", &[("", "hub_a", HUB_A), ("", "render", "names/render().")]);
    files.push(more);

    // A path longer than a path may be, one near the limit, and one with non-ASCII characters.
    let deep = format!("src/{}/deep.rs", vec!["d".repeat(100); 5].join("/"));
    let mut deep_file = SynFile::new(&deep);
    deep_file.line("// deep");
    deep_file.function("deep_fn", "deep/deep_fn().", &[("", "hub_a", HUB_A)]);
    files.push(deep_file);
    let near = format!("src/{}/near.rs", vec!["n".repeat(108); 4].join("/"));
    let mut near_file = SynFile::new(&near);
    near_file.line("// near the limit");
    near_file.function("near_fn", "near/near_fn().", &[("", "hub_a", HUB_A)]);
    files.push(near_file);
    let mut unicode = SynFile::new("src/ünï/cödé.rs");
    unicode.line("// unicode path");
    unicode.function("grüße", "uni/grüße().", &[("", "hub_a", HUB_A)]);
    files.push(unicode);

    // Covered, but nothing is defined in it.
    let mut empty = SynFile::new("src/empty.rs");
    empty.line("// nothing defined here");
    files.push(empty);

    // 1,000 short lines, to meet the row cap before the size budget.
    let mut lines = SynFile::new("src/lines.rs");
    for _ in 0..1000 {
        lines.line("x");
    }
    files.push(lines);

    let scip = write_synthetic(&root, &files, &[("src/uncovered.rs", "// no indexer saw this\n")]);
    index_from_scip(&root, &scip);
    Project { _dir: dir, root }
}

// ------------------------------------------------------------------------------------------
// The facts, read straight from the database without going through any tool.
// ------------------------------------------------------------------------------------------

struct NodeFact {
    id: String,
    kind: String,
    name: String,
    qualified: String,
    file: Option<String>,
    start: Option<i64>,
    end: Option<i64>,
    fan_in: i64,
    fan_out: i64,
    duplicate: bool,
}

struct FileFact {
    path: String,
    lines: i64,
    covered: bool,
    language: String,
}

struct EdgeFact {
    source: String,
    target: String,
    kind: String,
    count: i64,
    lines: Vec<i64>,
}

struct Facts {
    root: PathBuf,
    nodes: Vec<NodeFact>,
    edges: Vec<EdgeFact>,
    files: Vec<FileFact>,
    by_id: HashMap<String, usize>,
}

impl Facts {
    fn load(project: &Project) -> Facts {
        let db = open_read_only(&config::database_path(&project.root)).unwrap();
        let nodes: Vec<NodeFact> = db
            .prepare("SELECT id, type, name, file_path, start_line, end_line, metadata FROM nodes ORDER BY id")
            .unwrap()
            .query_map([], |r| {
                let metadata: Value = serde_json::from_str(&r.get::<_, String>(6)?).unwrap();
                Ok(NodeFact {
                    id: r.get(0)?,
                    kind: r.get(1)?,
                    name: r.get(2)?,
                    qualified: metadata["qualified"].as_str().unwrap_or("").to_string(),
                    file: r.get(3)?,
                    start: r.get(4)?,
                    end: r.get(5)?,
                    fan_in: metadata["fanIn"].as_i64().unwrap_or(0),
                    fan_out: metadata["fanOut"].as_i64().unwrap_or(0),
                    duplicate: metadata.get("duplicateOf").is_some(),
                })
            })
            .unwrap()
            .map(Result::unwrap)
            .collect();
        let edges: Vec<EdgeFact> = db
            .prepare("SELECT source_id, target_id, type, metadata FROM edges")
            .unwrap()
            .query_map([], |r| {
                let metadata: Value = serde_json::from_str(&r.get::<_, String>(3)?).unwrap();
                Ok(EdgeFact {
                    source: r.get(0)?,
                    target: r.get(1)?,
                    kind: r.get(2)?,
                    count: metadata["count"].as_i64().unwrap_or(1),
                    lines: metadata["lines"].as_array().map(|a| a.iter().filter_map(Value::as_i64).collect()).unwrap_or_default(),
                })
            })
            .unwrap()
            .map(Result::unwrap)
            .collect();
        let files = db
            .prepare("SELECT file_path, lines, indexed_by IS NOT NULL, language FROM file_manifest ORDER BY file_path")
            .unwrap()
            .query_map([], |r| Ok(FileFact { path: r.get(0)?, lines: r.get(1)?, covered: r.get(2)?, language: r.get(3)? }))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        let by_id = nodes.iter().enumerate().map(|(i, n)| (n.id.clone(), i)).collect();
        Facts { root: project.root.clone(), nodes, edges, files, by_id }
    }

    fn node(&self, id: &str) -> &NodeFact {
        &self.nodes[*self.by_id.get(id).unwrap_or_else(|| panic!("no node {id}"))]
    }

    fn symbols(&self) -> impl Iterator<Item = &NodeFact> {
        self.nodes.iter().filter(|n| SYMBOL_TYPES.contains(&n.kind.as_str()))
    }

    /// Lines of a source file, read directly from disk.
    fn source(&self, path: &str) -> Vec<String> {
        let content = fs::read_to_string(self.root.join(path)).unwrap();
        content.lines().map(String::from).collect()
    }
}

/// Evenly spaced items, so a sample covers the whole list and is the same on every run.
fn sample<T: Clone>(items: &[T], max: usize) -> Vec<T> {
    if items.len() <= max {
        return items.to_vec();
    }
    (0..max).map(|i| items[i * items.len() / max].clone()).collect()
}

fn addressable(id: &str) -> bool {
    id.chars().count() <= mcp::ID_MAX
}

// ------------------------------------------------------------------------------------------
// What a valid call looks like, per tool. Everything else is generic.
// ------------------------------------------------------------------------------------------

fn cases(tool: &str, f: &Facts) -> Vec<Value> {
    // A query is at most 200 characters; longer names are looked up by their id instead.
    let names: BTreeSet<&str> = f.symbols().map(|n| n.name.as_str()).filter(|n| n.chars().count() <= 200).collect();
    let names: Vec<&str> = names.into_iter().collect();
    let ids: Vec<&str> = f.nodes.iter().map(|n| n.id.as_str()).filter(|id| addressable(id)).collect();
    match tool {
        "overview" => vec![json!({})],
        "find_symbols" => {
            let mut out = vec![json!({"query": "%"}), json!({"query": "_"}), json!({"query": "no_such_symbol_zzz"})];
            for name in sample(&names, 25) {
                out.push(json!({"query": name}));
                out.push(json!({"query": name.to_uppercase()}));
                out.push(json!({"query": name.chars().take(2).collect::<String>()}));
            }
            let kinds: BTreeSet<&str> = f.symbols().map(|n| n.kind.as_str()).collect();
            for kind in kinds {
                out.push(json!({"query": "e", "kind": kind}));
            }
            let dirs: BTreeSet<String> = f
                .files
                .iter()
                .map(|file| {
                    let p = &file.path;
                    p.split('/').next().unwrap().to_string() + if p.contains('/') { "/" } else { "" }
                })
                .collect();
            for dir in dirs {
                out.push(json!({"query": "a", "path_prefix": dir}));
            }
            out
        }
        "file_outline" => sample(&f.files.iter().collect::<Vec<_>>(), 40)
            .into_iter()
            .filter(|file| file.path.chars().count() <= mcp::PATH_MAX)
            .map(|file| json!({"path": file.path}))
            .collect(),
        "symbol_detail" => {
            let mut chosen: BTreeSet<&str> = sample(&ids, 60).into_iter().collect();
            let mut by_fan: Vec<&NodeFact> = f.nodes.iter().filter(|n| addressable(&n.id)).collect();
            by_fan.sort_by_key(|n| std::cmp::Reverse(n.fan_in));
            chosen.extend(by_fan.iter().take(3).map(|n| n.id.as_str()));
            chosen.extend(ids.iter().copied().filter(|id| id.contains('~')));
            chosen.extend(f.nodes.iter().filter(|n| n.fan_in == 0 && n.fan_out == 0).take(3).map(|n| n.id.as_str()));
            for kind in ["EXTERNAL", "MODULE", "FILE"] {
                chosen.extend(f.nodes.iter().filter(|n| n.kind == kind && addressable(&n.id)).take(2).map(|n| n.id.as_str()));
            }
            chosen.into_iter().map(|id| json!({"id": id})).collect()
        }
        "trace" => {
            let mut chosen: BTreeSet<&str> = sample(&ids, 20).into_iter().collect();
            let mut by_fan: Vec<&NodeFact> = f.nodes.iter().filter(|n| addressable(&n.id)).collect();
            by_fan.sort_by_key(|n| std::cmp::Reverse(n.fan_in));
            chosen.extend(by_fan.iter().take(2).map(|n| n.id.as_str()));
            chosen.extend(f.nodes.iter().filter(|n| n.kind == "EXTERNAL").take(1).map(|n| n.id.as_str()));
            let mut out = Vec::new();
            for id in chosen {
                for direction in ["dependents", "dependencies"] {
                    for edges in ["calls", "all"] {
                        for depth in [1, 3] {
                            out.push(json!({"id": id, "direction": direction, "edges": edges, "depth": depth}));
                        }
                    }
                }
            }
            out
        }
        "read_source" => {
            let mut out = Vec::new();
            for file in sample(&f.files.iter().collect::<Vec<_>>(), 30) {
                // An empty file has no line to read; that refusal is tested on its own.
                if file.lines == 0 || file.path.chars().count() > mcp::PATH_MAX {
                    continue;
                }
                let (path, lines) = (&file.path, file.lines);
                out.push(json!({"path": path, "start": 1, "end": lines.clamp(1, 10)}));
                out.push(json!({"path": path, "start": 1, "end": lines.clamp(1, 400)}));
                out.push(json!({"path": path, "start": lines.max(1), "end": lines.max(1) + 5}));
            }
            out
        }
        other => panic!("tool `{other}` has no conformance cases: add them to `cases` in conformance.rs"),
    }
}

// ------------------------------------------------------------------------------------------
// The rules every answer must follow, whatever the tool.
// ------------------------------------------------------------------------------------------

fn text_of(result: &Value) -> &str {
    result["content"][0]["text"].as_str().unwrap_or("")
}

fn check_response(def: &ToolDef, offset: i64, budget_bytes: usize, result: &Value) -> Result<Value, String> {
    if result["isError"] != json!(false) {
        return Err(format!("an error result: {}", text_of(result)));
    }
    let text = result["content"][0]["text"].as_str().ok_or("no text content")?;
    if text.len() > budget_bytes {
        return Err(format!("{} bytes exceed the budget of {budget_bytes}", text.len()));
    }
    let value: Value = serde_json::from_str(text).map_err(|e| format!("the text is not JSON: {e}"))?;
    if result.get("structuredContent") != Some(&value) {
        return Err("structuredContent is not the same as the text".into());
    }
    schema::validate(&def.output_schema(), &value).map_err(|e| format!("breaks the output schema: {}", e.join("; ")))?;
    if value["as_of"].as_str().is_none_or(str::is_empty) {
        return Err("as_of is empty".into());
    }
    let notes: Vec<&str> = value["notes"].as_array().map(|n| n.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
    if notes.len() > 12 {
        return Err(format!("{} notes", notes.len()));
    }
    for spec in def.tables {
        let table = &value[spec.name];
        let rows = table["rows"].as_array().ok_or("rows is not an array")?;
        let (shown, total) = (table["shown"].as_i64().ok_or("no shown")?, table["total"].as_i64().ok_or("no total")?);
        let names: Vec<&str> = table["columns"].as_array().ok_or("no columns")?.iter().filter_map(Value::as_str).collect();
        if names != spec.columns.iter().map(|c| c.name).collect::<Vec<_>>() {
            return Err(format!("`{}` has the wrong columns: {names:?}", spec.name));
        }
        if shown != rows.len() as i64 {
            return Err(format!("`{}` says shown={shown} but holds {} rows", spec.name, rows.len()));
        }
        if rows.len() as i64 > WINDOW {
            return Err(format!("`{}` holds {} rows, more than the {WINDOW} allowed", spec.name, rows.len()));
        }
        let start = if def.paginated { offset } else { 0 };
        if !rows.is_empty() && start + shown > total {
            return Err(format!("`{}` shows rows {}..{} of only {total}", spec.name, start, start + shown));
        }
        let next = &table["next_offset"];
        if def.paginated {
            let more = start + shown < total;
            match (more, next.as_i64()) {
                (true, Some(n)) if n == start + shown && shown >= 1 => {}
                (true, _) => return Err(format!("`{}` has more rows but next_offset is {next} after {shown} rows", spec.name)),
                (false, None) if next.is_null() => {}
                (false, _) => return Err(format!("`{}` is complete but next_offset is {next}", spec.name)),
            }
        } else {
            if !next.is_null() {
                return Err(format!("`{}` is not paginated but has a next_offset", spec.name));
            }
            let announced = notes.iter().any(|n| n.contains(&format!("`{}` shows {shown} of {total}", spec.name)));
            if shown < total && !announced {
                return Err(format!("`{}` shows {shown} of {total} rows without saying so", spec.name));
            }
        }
    }
    Ok(value)
}

fn is_budget_error(message: &str) -> bool {
    message.contains("does not fit the budget") || message.contains("make progress")
}

/// Follows `next_offset` until the table is complete, checking every page. `small` fits each page
/// to a budget smaller than any caller may ask for, so that paging happens on small data. `Err`
/// only if that budget cannot hold some row; any other problem fails the test.
fn try_pages(server: &mut Server, def: &ToolDef, base: &Value, small: Option<usize>) -> Result<Vec<Value>, String> {
    let table = def.tables[0].name;
    let mut args = base.clone();
    let (mut offset, mut out) = (0i64, Vec::new());
    loop {
        args["offset"] = json!(offset);
        let (result, budget) = match small {
            Some(bytes) => (server.call_tool_with_budget_bytes(def.name, &args, bytes), bytes),
            None => {
                let tokens = args.get("budget_tokens").and_then(Value::as_i64).unwrap_or(DEFAULT_BUDGET_TOKENS);
                (server.call_tool(def.name, &args), tokens as usize * BYTES_PER_TOKEN)
            }
        };
        if result["isError"] == json!(true) && is_budget_error(text_of(&result)) {
            return Err(text_of(&result).to_string());
        }
        let answer = check_response(def, offset, budget, &result).unwrap_or_else(|e| panic!("{} {args}: {e}", def.name));
        let next = answer[table]["next_offset"].as_i64();
        out.push(answer);
        match next {
            Some(n) => offset = n,
            None => return Ok(out),
        }
        assert!(out.len() < 100_000, "paging does not end for {args}");
    }
}

fn pages(server: &mut Server, def: &ToolDef, base: &Value, small: Option<usize>) -> Vec<Value> {
    try_pages(server, def, base, small).unwrap_or_else(|e| panic!("{} {base}: {e}", def.name))
}

fn rows_of(pages: &[Value], table: &str) -> Vec<Vec<Value>> {
    pages
        .iter()
        .flat_map(|p| p[table]["rows"].as_array().unwrap().iter().map(|r| r.as_array().unwrap().clone()))
        .collect()
}

fn tool_def(name: &str) -> ToolDef {
    tools::registry().into_iter().find(|t| t.name == name).unwrap()
}

fn str_at(row: &[Value], index: usize) -> &str {
    row[index].as_str().unwrap_or_default()
}

// ------------------------------------------------------------------------------------------
// Tests: the rules, for every tool and every project.
// ------------------------------------------------------------------------------------------

#[test]
fn every_registered_tool_has_conformance_cases() {
    let project = fixture_project("ts");
    let facts = Facts::load(&project);
    for def in tools::registry() {
        assert!(!cases(def.name, &facts).is_empty(), "{} has no cases", def.name);
    }
    let unknown = std::panic::catch_unwind(|| cases("a_tool_nobody_wrote_cases_for", &Facts::load(&fixture_project("ts"))));
    assert!(unknown.is_err(), "a tool without cases must fail the suite, not pass it");
}

fn valid_calls_obey_the_contract(project: &Project) {
    let facts = Facts::load(project);
    let mut server = new_server(project);
    for def in tools::registry() {
        for args in cases(def.name, &facts) {
            for tokens in [MIN_BUDGET_TOKENS, DEFAULT_BUDGET_TOKENS, MAX_BUDGET_TOKENS] {
                let mut call = args.clone();
                call["budget_tokens"] = json!(tokens);
                let first = server.call_tool(def.name, &call);
                let offset = 0;
                check_response(&def, offset, tokens as usize * BYTES_PER_TOKEN, &first)
                    .unwrap_or_else(|e| panic!("{} {call}: {e}", def.name));
                // The same question gives the same bytes, from this server and from a new one.
                assert_eq!(text_of(&first), text_of(&server.call_tool(def.name, &call)), "{} {call} is not repeatable", def.name);
            }
        }
    }
    let mut fresh = new_server(project);
    for def in tools::registry() {
        for args in cases(def.name, &facts).into_iter().take(10) {
            assert_eq!(
                text_of(&server.call_tool(def.name, &args)),
                text_of(&fresh.call_tool(def.name, &args)),
                "{} {args}: two servers disagree",
                def.name
            );
        }
    }
}

#[test]
fn valid_calls_obey_the_contract_on_real_indexes() {
    for name in REAL {
        valid_calls_obey_the_contract(&fixture_project(name));
    }
}

#[test]
fn valid_calls_obey_the_contract_at_scale() {
    valid_calls_obey_the_contract(&synthetic_project());
}

/// The smallest of `candidates` that holds every page of this call, then all its pages.
fn small_pages(server: &mut Server, def: &ToolDef, args: &Value, candidates: &[usize]) -> Vec<Value> {
    for &bytes in candidates {
        if let Ok(found) = try_pages(server, def, args, Some(bytes)) {
            return found;
        }
    }
    panic!("{} {args}: no candidate budget in {candidates:?} holds every page", def.name);
}

fn pages_join_up(project: &Project, candidates: &[usize], max_cases: usize, extra: &[(&str, Value)], must_page: bool) {
    let facts = Facts::load(project);
    let mut server = new_server(project);
    for def in tools::registry().into_iter().filter(|d| d.paginated) {
        let table = def.tables[0].name;
        let mut multi_page = 0;
        let mut chosen = sample(&cases(def.name, &facts), max_cases);
        chosen.extend(extra.iter().filter(|(tool, _)| *tool == def.name).map(|(_, args)| args.clone()));
        let chosen_is_empty = chosen.is_empty();
        for args in chosen {
            let small = small_pages(&mut server, &def, &args, candidates);
            let mut big = args.clone();
            big["budget_tokens"] = json!(MAX_BUDGET_TOKENS);
            let large = pages(&mut server, &def, &big, None);
            let (a, b) = (rows_of(&small, table), rows_of(&large, table));
            assert_eq!(a, b, "{} {args}: the rows depend on the page size", def.name);
            let totals: HashSet<i64> = small.iter().chain(&large).map(|p| p[table]["total"].as_i64().unwrap()).collect();
            assert_eq!(totals.len(), 1, "{} {args}: total changed between pages", def.name);
            assert_eq!(a.len() as i64, *totals.iter().next().unwrap(), "{} {args}: pages do not add up to the total", def.name);
            if small.len() > 1 {
                multi_page += 1;
            }
        }
        if must_page && !chosen_is_empty {
            assert!(multi_page > 0, "{}: no case needed more than one page, so paging was never exercised", def.name);
        }
    }
}

#[test]
fn pages_join_up_exactly_on_real_indexes() {
    for name in REAL {
        pages_join_up(&fixture_project(name), &[700, 900, 1_200, 1_600], usize::MAX, &[], true);
    }
}

#[test]
fn pages_join_up_exactly_at_scale() {
    // Two ways in a chain and across modules lead back from f25 of module 5: dozens of nodes.
    let deep = json!({"id": "cargo:synth:m5/f25().", "direction": "dependents", "edges": "all", "depth": 4});
    let project = synthetic_project();
    pages_join_up(&project, &[3_000, 3_600], 10, &[], false);
    pages_join_up(&project, &[1_000, 1_200, 1_600], 0, &[("trace", deep)], true);
}


// ------------------------------------------------------------------------------------------
// Inputs that should be refused, and text that should do nothing.
// ------------------------------------------------------------------------------------------

/// A hash of every file under `root`, ignoring SQLite's own side files.
fn digest(root: &Path) -> String {
    fn walk(dir: &Path, base: &Path, out: &mut Vec<String>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, base, out);
            } else if !path.to_string_lossy().ends_with("-wal") && !path.to_string_lossy().ends_with("-shm") {
                let relative = path.strip_prefix(base).unwrap().to_string_lossy().into_owned();
                out.push(format!("{relative}:{}", file_hash(&fs::read(&path).unwrap())));
            }
        }
    }
    let mut entries = Vec::new();
    walk(root, root, &mut entries);
    entries.sort();
    file_hash(entries.join("\n").as_bytes())
}

fn base_call(def: &ToolDef, facts: &Facts) -> Value {
    cases(def.name, facts).into_iter().next().unwrap_or_else(|| panic!("{} has no cases", def.name))
}

#[test]
fn bad_inputs_are_refused_by_every_tool_with_a_message_naming_the_problem() {
    let project = fixture_project("ts");
    let facts = Facts::load(&project);
    let mut server = new_server(&project);
    let refused = |server: &mut Server, def: &ToolDef, call: &Value, expected: &str| {
        let result = server.call_tool(def.name, call);
        let text = text_of(&result);
        assert_eq!(result["isError"], json!(true), "{} {call} must be refused", def.name);
        assert!(text.contains(expected), "{} {call}: expected {expected:?} in {text:?}", def.name);
        assert!(!text.starts_with("internal error"), "{} {call}: {text}", def.name);
        assert!(text.len() < 1500, "{} {call}: the message is {} bytes", def.name, text.len());
        assert!(result.get("structuredContent").is_none(), "a refusal carries no data");
    };
    for def in tools::registry() {
        let base = base_call(&def, &facts);
        assert!(!text_of(&server.call_tool(def.name, &base)).is_empty());
        for param in def.all_params() {
            let mut bad: Vec<Value> = Vec::new();
            match &param.kind {
                ParamKind::Text { min, max } => {
                    bad.extend([json!(1), json!(1.5), json!(true), json!(null), json!([]), json!({}), json!(["a"])]);
                    if *min > 0 {
                        bad.push(json!(""));
                    }
                    bad.push(json!("x".repeat(max + 1)));
                    bad.push(json!("é".repeat(max + 1)));
                }
                ParamKind::Int { min, max } => {
                    bad.extend([json!("1"), json!(1.5), json!(1.0), json!(true), json!(null), json!([]), json!({}), json!(min - 1), json!(max + 1)]);
                    bad.push(json!(i64::MAX));
                    bad.push(json!(i64::MIN));
                }
                ParamKind::Choice(values) => {
                    let flipped = if values[0] == values[0].to_uppercase() { values[0].to_lowercase() } else { values[0].to_uppercase() };
                    bad.extend([json!(1), json!(null), json!("NOPE"), json!(flipped), json!(format!("{} ", values[0])), json!("")]);
                }
            }
            for value in bad {
                let mut call = base.clone();
                call[param.name] = value;
                refused(&mut server, &def, &call, &format!("$.{}", param.name));
            }
            if param.required {
                let mut call = base.clone();
                call.as_object_mut().unwrap().remove(param.name);
                refused(&mut server, &def, &call, &format!("missing required `{}`", param.name));
            }
        }
        let mut call = base.clone();
        call["zzz_unknown"] = json!(1);
        refused(&mut server, &def, &call, "unknown parameter `zzz_unknown`");
        let mut call = base.clone();
        call["Query"] = json!("wrong case of a real name");
        refused(&mut server, &def, &call, "unknown parameter `Query`");
        // Arguments that are not an object never reach the tool.
        for arguments in [json!([]), json!("x"), json!(5), json!(true)] {
            let reply = server
                .handle_message(json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": def.name, "arguments": arguments}}))
                .unwrap();
            assert_eq!(reply["error"]["code"], -32602, "{} {arguments}", def.name);
        }
    }
}

const HOSTILE: &[&str] = &[
    "'; DROP TABLE nodes; --",
    "\" OR 1=1 --",
    "' OR '1'='1",
    "%",
    "%%",
    "_",
    "\\",
    "\\%",
    "a%b_c\\d",
    "../../../etc/passwd",
    "..\\..\\windows",
    "/etc/passwd",
    "C:\\Windows\\System32",
    "src/../../x",
    "./src/./shapes.ts",
    "src//shapes.ts",
    "src/shapes.ts/",
    "a\nb",
    "\t",
    " ",
    "  padded  ",
    "\u{202e}rtl",
    "𝔘𝔫𝔦𝔠𝔬𝔡𝔢",
    "\u{feff}bom",
    "\u{7f}",
    "\u{1}\u{2}",
    "null",
    "undefined",
    "{{7*7}}",
    "${7*7}",
    "<script>alert(1)</script>",
    "*",
    "?",
    "[",
    "(",
    "\"",
    "'",
    "`",
    "{\"method\":\"tools/call\"}",
];

fn hostile_values(min: usize, max: usize) -> Vec<String> {
    let mut values: Vec<String> = HOSTILE.iter().map(|s| s.chars().take(max).collect::<String>()).collect();
    values.push("👋".repeat(max));
    values.push("é".repeat(max));
    values.push("\"".repeat(max));
    values.push("\\".repeat(max));
    values.push("x".repeat(max));
    values.retain(|v| v.chars().count() >= min);
    values
}

fn hostile_text_never_gets_through(project: &Project) {
    let facts = Facts::load(project);
    let mut server = new_server(project);
    let before = digest(&project.root);
    for def in tools::registry() {
        let base = base_call(&def, &facts);
        for param in def.params.iter() {
            let ParamKind::Text { min, max } = param.kind else { continue };
            for value in hostile_values(min, max) {
                let mut call = base.clone();
                call[param.name] = json!(value);
                let result = server.call_tool(def.name, &call);
                if result["isError"] == json!(true) {
                    let text = text_of(&result);
                    assert!(!text.starts_with("internal error"), "{} {call}: {text}", def.name);
                    assert!(text.len() < 1500, "{}: error of {} bytes", def.name, text.len());
                } else {
                    check_response(&def, 0, DEFAULT_BUDGET_TOKENS as usize * BYTES_PER_TOKEN, &result)
                        .unwrap_or_else(|e| panic!("{} {call}: {e}", def.name));
                }
            }
            // A NUL character is refused outright, everywhere.
            let mut call = base.clone();
            call[param.name] = json!(format!("a{}b", '\u{0}'));
            let result = server.call_tool(def.name, &call);
            assert!(result["isError"] == json!(true) && text_of(&result).contains("NUL"), "{} {call}", def.name);
        }
    }
    assert_eq!(digest(&project.root), before, "nothing the tools were sent changed the project or its index");
}

#[test]
fn hostile_text_never_gets_through_or_breaks_a_tool() {
    hostile_text_never_gets_through(&fixture_project("ts"));
    hostile_text_never_gets_through(&fixture_project("py"));
    hostile_text_never_gets_through(&synthetic_project());
}

#[test]
fn paths_cannot_reach_files_outside_the_index() {
    let project = fixture_project("ts");
    // A real file next to the project, and a file inside it that was never indexed.
    fs::write(project.root.parent().unwrap().join("secret.txt"), "outside").unwrap();
    fs::write(project.root.join("notes.txt"), "inside but not source").unwrap();
    let mut server = new_server(&project);
    for path in [
        "../secret.txt",
        "src/../../secret.txt",
        "/etc/passwd",
        "notes.txt",
        "./notes.txt",
        "src",
        "src/",
        ".codebase-context/graph.db",
        ".codebase-context/config.json",
        "package.json",
        "node_modules/x.js",
    ] {
        for (tool, args) in [
            ("read_source", json!({"path": path, "start": 1, "end": 5})),
            ("file_outline", json!({"path": path})),
        ] {
            let result = server.call_tool(tool, &args);
            assert_eq!(result["isError"], json!(true), "{tool} {path} must not be served: {}", text_of(&result));
            assert!(!text_of(&result).contains("outside") && !text_of(&result).contains("inside but not"), "{tool} {path} leaked content");
        }
    }
}

// ------------------------------------------------------------------------------------------
// Oracles: the same answer worked out another way.
// ------------------------------------------------------------------------------------------

fn priority(kind: &str) -> u8 {
    match kind {
        "CALLS" => 0,
        "IMPLEMENTS" => 1,
        "REFERENCES" => 2,
        "USES" => 3,
        _ => 4,
    }
}

struct Expected {
    from: i64,
    depth: i64,
    rel: String,
    id: String,
    count: i64,
    lines: Vec<i64>,
}

fn other(edge: &EdgeFact, dependents: bool) -> &str {
    if dependents { &edge.source } else { &edge.target }
}

/// Breadth-first closure over all edges held in memory, ordered by the rule the tool documents.
fn naive_closure(f: &Facts, root: &str, dependents: bool, calls_only: bool, depth: i64, cap: usize) -> (Vec<Expected>, bool) {
    let allowed: &[&str] = if calls_only { &["CALLS"] } else { &["CALLS", "REFERENCES", "IMPLEMENTS", "USES"] };
    let mut adjacent: HashMap<&str, Vec<&EdgeFact>> = HashMap::new();
    for edge in f.edges.iter().filter(|e| allowed.contains(&e.kind.as_str())) {
        let key = if dependents { edge.target.as_str() } else { edge.source.as_str() };
        adjacent.entry(key).or_default().push(edge);
    }
    for list in adjacent.values_mut() {
        list.sort_by(|a, b| {
            (priority(&a.kind), std::cmp::Reverse(a.count), other(a, dependents))
                .cmp(&(priority(&b.kind), std::cmp::Reverse(b.count), other(b, dependents)))
        });
    }
    let mut seen: HashSet<&str> = HashSet::from([root]);
    let mut out: Vec<Expected> = Vec::new();
    let mut frontier: Vec<(&str, i64)> = vec![(root, 0)];
    for level in 1..=depth {
        let mut next = Vec::new();
        for (parent, parent_n) in &frontier {
            for edge in adjacent.get(parent).map(Vec::as_slice).unwrap_or_default() {
                let id = other(edge, dependents);
                if seen.contains(id) {
                    continue;
                }
                if out.len() >= cap {
                    return (out, false);
                }
                seen.insert(id);
                out.push(Expected {
                    from: *parent_n,
                    depth: level,
                    rel: edge.kind.clone(),
                    id: id.to_string(),
                    count: edge.count,
                    lines: edge.lines.clone(),
                });
                next.push((id, out.len() as i64));
            }
        }
        if next.is_empty() {
            break;
        }
        frontier = next;
    }
    (out, true)
}

/// Equal, or the answer's copy is the clipped form of the real value (which the answer reports).
fn assert_same(got: &Value, want: &Value, at: &str) {
    if got == want {
        return;
    }
    let (Some(g), Some(w)) = (got.as_str(), want.as_str()) else { panic!("{at}: {got} != {want}") };
    let body = g.strip_suffix('…').unwrap_or_else(|| panic!("{at}: {g:?} != {w:?}"));
    assert!(w.starts_with(body) && w.chars().count() > body.chars().count(), "{at}: {g:?} is not a clipped form of {w:?}");
}

fn trace_matches_naive(project: &Project, args_list: Vec<Value>, candidates: &[usize]) {
    let facts = Facts::load(project);
    let mut server = new_server(project);
    let def = tool_def("trace");
    for args in args_list {
        let root = args["id"].as_str().unwrap();
        let dependents = args["direction"] == json!("dependents");
        let (calls_only, depth) = (args["edges"] == json!("calls"), args["depth"].as_i64().unwrap());
        let (expected, complete) = naive_closure(&facts, root, dependents, calls_only, depth, tools::MAX_VISITED);

        let answer_pages = small_pages(&mut server, &def, &args, candidates);
        let head = &answer_pages[0];
        assert_eq!(head["complete"], json!(complete), "{args}: complete");
        assert_eq!(head["id"], json!(root));
        let rows = rows_of(&answer_pages, "nodes");
        assert_eq!(rows.len(), expected.len(), "{args}: number of nodes");
        let mut levels = vec![0i64; expected.iter().map(|e| e.depth).max().unwrap_or(0) as usize];
        for (index, (row, want)) in rows.iter().zip(&expected).enumerate() {
            let at = format!("{args} row {}", index + 1);
            let node = facts.node(&want.id);
            assert_eq!(row[0], json!(index as i64 + 1), "{at}: n");
            assert_eq!(row[1], json!(want.from), "{at}: from");
            assert_eq!(row[2], json!(want.depth), "{at}: depth");
            assert_eq!(row[3], json!(want.rel), "{at}: rel");
            assert_same(&row[4], &json!(want.id), &format!("{at}: id"));
            assert_eq!(row[5], json!(node.kind), "{at}: kind");
            assert_same(&row[6], &json!(node.name), &format!("{at}: name"));
            assert_same(&row[7], &json!(node.file), &format!("{at}: file"));
            assert_eq!(row[8], json!(node.start), "{at}: line");
            assert_eq!(row[9], json!(want.count), "{at}: count");
            assert_eq!(row[10], json!(want.lines.iter().take(5).collect::<Vec<_>>()), "{at}: sites");
            levels[want.depth as usize - 1] += 1;

            // The reference really is where the answer says it is: the line of the file that
            // makes the reference mentions the thing that is referenced.
            let parent = if want.from == 0 { root } else { expected[want.from as usize - 1].id.as_str() };
            let (referencing, referenced) = if dependents { (want.id.as_str(), parent) } else { (parent, want.id.as_str()) };
            let referenced = facts.node(referenced);
            if referenced.kind == "EXTERNAL" {
                continue; // the line names an item of the package, not the package
            }
            let referencing_file = facts.node(referencing).file.clone().expect("a referencing node has a file");
            let source = facts.source(&referencing_file);
            for line in row[10].as_array().unwrap().iter().filter_map(Value::as_i64) {
                let text = source.get(line as usize - 1).unwrap_or_else(|| panic!("{at}: line {line} is past the end of {referencing_file}"));
                assert!(text.contains(&referenced.name), "{at}: line {line} of {referencing_file} is {text:?}, which does not mention {}", referenced.name);
            }
        }
        assert_eq!(head["levels"], json!(levels), "{args}: levels");
        assert_eq!(answer_pages[0]["nodes"]["total"], json!(expected.len()), "{args}: total");
    }
}

#[test]
fn trace_agrees_with_a_naive_search_node_for_node_on_real_indexes() {
    for name in REAL {
        let project = fixture_project(name);
        let facts = Facts::load(&project);
        let mut all = Vec::new();
        for node in facts.nodes.iter().filter(|n| addressable(&n.id)) {
            for direction in ["dependents", "dependencies"] {
                for edges in ["calls", "all"] {
                    for depth in [1, 2, 4] {
                        all.push(json!({"id": node.id, "direction": direction, "edges": edges, "depth": depth}));
                    }
                }
            }
        }
        trace_matches_naive(&project, all, &[700, 900, 1_200, 1_600]);
    }
}

#[test]
fn trace_agrees_with_a_naive_search_at_scale_including_the_node_cap() {
    let project = synthetic_project();
    let id = |descriptors: &str| format!("cargo:synth:{descriptors}");
    let mut args = Vec::new();
    for (node, depth) in [
        ("hub/hub_a().", 1),
        ("hub/hub_a().", 3),
        ("hub/hub_b().", 2),
        ("m5/f25().", 4),
        ("m0/f0().", 4),
        ("m129/f49().", 3),
        ("odd/orphan().", 4),
        ("dup/dup_fn().", 2),
        ("names/render().", 3),
    ] {
        for direction in ["dependents", "dependencies"] {
            for edges in ["calls", "all"] {
                args.push(json!({"id": id(node), "direction": direction, "edges": edges, "depth": depth}));
            }
        }
    }
    trace_matches_naive(&project, args, &[12_000]);

    // The cap itself: more than 5,000 callers, so the walk stops, says so, and is still exact.
    let mut server = new_server(&project);
    let def = tool_def("trace");
    let args = json!({"id": id("hub/hub_a()."), "direction": "dependents", "edges": "calls", "depth": 1, "budget_tokens": MAX_BUDGET_TOKENS});
    let first = check_response(&def, 0, MAX_BUDGET_TOKENS as usize * BYTES_PER_TOKEN, &server.call_tool("trace", &args)).unwrap();
    assert_eq!(first["complete"], json!(false));
    assert_eq!(first["nodes"]["total"], json!(tools::MAX_VISITED));
    assert!(first["notes"].to_string().contains("stopped after 5000 nodes"), "{}", first["notes"]);
}

#[test]
fn every_location_an_outline_gives_holds_the_symbol_it_names() {
    for project in [fixture_project("ts"), fixture_project("py"), fixture_project("rust"), synthetic_project()] {
        let facts = Facts::load(&project);
        let mut server = new_server(&project);
        let def = tool_def("file_outline");
        let mut checked = 0;
        for file in facts.files.iter().filter(|f| f.path.chars().count() <= mcp::PATH_MAX) {
            let outline = pages(&mut server, &def, &json!({"path": file.path, "budget_tokens": MAX_BUDGET_TOKENS}), None);
            let source = facts.source(&file.path);
            assert_eq!(outline[0]["covered"], json!(file.covered), "{}", file.path);
            assert_eq!(outline[0]["lines"], json!(file.lines), "{}", file.path);
            for row in rows_of(&outline, "symbols") {
                // A name too long to show whole comes back clipped; its beginning must still be there.
                let (name, start, end) = (str_at(&row, 1).trim_end_matches('…'), row[2].as_i64().unwrap(), row[3].as_i64().unwrap());
                assert!(1 <= start && start <= end && end <= file.lines, "{}: {name} spans {start}..{end} of {} lines", file.path, file.lines);
                let text = source[start as usize - 1..end as usize].join("\n");
                assert!(text.contains(name), "{}: lines {start}..{end} for {name} are {text:?}", file.path);
                checked += 1;
            }
        }
        assert!(checked > 20, "only {checked} locations were checked");
    }
}

fn naive_find<'a>(f: &'a Facts, query: &str, kind: Option<&str>, prefix: Option<&str>) -> Vec<&'a NodeFact> {
    let q = query.to_ascii_lowercase();
    let mut found: Vec<&NodeFact> = f
        .symbols()
        .filter(|n| n.name.to_ascii_lowercase().contains(&q) || n.qualified.to_ascii_lowercase().contains(&q))
        .filter(|n| kind.is_none_or(|k| n.kind == k))
        .filter(|n| prefix.is_none_or(|p| n.file.as_deref().is_some_and(|file| file.starts_with(p))))
        .collect();
    found.sort_by(|a, b| {
        let key = |n: &NodeFact| {
            (
                !n.name.eq_ignore_ascii_case(query),
                !n.name.to_ascii_lowercase().starts_with(&q),
                std::cmp::Reverse(n.fan_in),
                n.name.clone(),
                n.id.clone(),
            )
        };
        key(a).cmp(&key(b))
    });
    found
}

fn find_symbols_matches_naive(project: &Project, candidates: &[usize], max_queries: usize) {
    let facts = Facts::load(project);
    let mut server = new_server(project);
    let def = tool_def("find_symbols");
    let names: BTreeSet<&str> = facts.symbols().map(|n| n.name.as_str()).filter(|n| n.chars().count() <= 200).collect();
    let names: Vec<&str> = names.into_iter().collect();
    let mut queries: Vec<(String, Option<String>, Option<String>)> = sample(&names, max_queries)
        .into_iter()
        .flat_map(|n| [n.to_string(), n.to_uppercase(), n.chars().take(3).collect()])
        .map(|q| (q, None, None))
        .collect();
    for q in ["%", "%%", "_", "\\", "a%b", "e", "E", "er", "r_", "no_such_name_anywhere", "\"", "'"] {
        queries.push((q.to_string(), None, None));
    }
    let kinds: BTreeSet<&str> = facts.symbols().map(|n| n.kind.as_str()).collect();
    for kind in kinds {
        queries.push(("e".to_string(), Some(kind.to_string()), None));
    }
    for file in facts.files.iter().take(6) {
        if let Some((dir, _)) = file.path.rsplit_once('/').filter(|(dir, _)| dir.chars().count() < mcp::PATH_MAX) {
            queries.push(("a".to_string(), None, Some(format!("{dir}/"))));
            queries.push(("a".to_string(), None, Some(format!("{}/", dir.to_uppercase()))));
        }
    }
    for (query, kind, prefix) in queries {
        let mut args = json!({"query": query});
        if let Some(kind) = &kind {
            args["kind"] = json!(kind);
        }
        if let Some(prefix) = &prefix {
            args["path_prefix"] = json!(prefix);
        }
        let expected = naive_find(&facts, &query, kind.as_deref(), prefix.as_deref());
        let got = small_pages(&mut server, &def, &args, candidates);
        assert_eq!(got[0]["symbols"]["total"], json!(expected.len()), "{args}: total");
        let rows = rows_of(&got, "symbols");
        let ids: Vec<&str> = rows.iter().map(|r| str_at(r, 0)).collect();
        let wanted: Vec<&str> = expected.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(ids.len(), wanted.len(), "{args}: how many");
        // Ids that do not fit in a row are clipped, so compare those by their start.
        for (index, (got_id, want_id)) in ids.iter().zip(&wanted).enumerate() {
            let clipped = got_id.ends_with('…');
            assert!(
                if clipped { want_id.starts_with(got_id.trim_end_matches('…')) } else { got_id == want_id },
                "{args}: result {index} is {got_id:?}, expected {want_id:?}"
            );
        }
        if expected.is_empty() && prefix.is_none() && kind.is_none() {
            assert!(got[0]["notes"].to_string().contains("no symbol matched"), "{args}: an empty result says why");
        }
    }
}

#[test]
fn find_symbols_agrees_with_a_naive_search_on_real_indexes() {
    for name in REAL {
        find_symbols_matches_naive(&fixture_project(name), &[700, 900, 1_600], usize::MAX);
    }
}

#[test]
fn find_symbols_agrees_with_a_naive_search_at_scale() {
    find_symbols_matches_naive(&synthetic_project(), &[3_000, 3_600], 8);
}

#[test]
fn outlines_list_exactly_the_symbols_the_database_holds_for_the_file() {
    for (index, project) in [fixture_project("ts"), fixture_project("py"), fixture_project("rust"), synthetic_project()].iter().enumerate() {
        let facts = Facts::load(project);
        // The tie-break between symbols that start on the same line can only be seen when the data
        // has such a pair, so the TypeScript and Rust fixtures must (see `Pair` in their shapes files).
        if index == 0 || index == 2 {
            let symbols: Vec<&NodeFact> = facts.nodes.iter().filter(|n| n.file.is_some() && n.kind != "FILE").collect();
            let tie = symbols.iter().any(|a| symbols.iter().any(|b| a.file == b.file && a.start == b.start && a.end != b.end));
            assert!(tie, "no two symbols start on one line and end on different lines, so the outline order is untested");
        }
        let mut server = new_server(&project);
        let def = tool_def("file_outline");
        for file in facts.files.iter().filter(|f| f.path.chars().count() <= mcp::PATH_MAX) {
            let mut expected: Vec<&NodeFact> =
                facts.nodes.iter().filter(|n| n.file.as_deref() == Some(&file.path) && n.kind != "FILE").collect();
            expected.sort_by_key(|n| (n.start, std::cmp::Reverse(n.end), n.id.clone()));
            let got = small_pages(&mut server, &def, &json!({"path": file.path}), &[1_000, 1_600, 3_000]);
            assert_eq!(got[0]["symbols"]["total"], json!(expected.len()), "{}", file.path);
            let rows = rows_of(&got, "symbols");
            assert_eq!(rows.len(), expected.len(), "{}", file.path);
            for (row, want) in rows.iter().zip(&expected) {
                assert_same(&row[4], &json!(want.id), &file.path);
                assert_eq!(row[0], json!(want.kind));
                assert_eq!(row[2], json!(want.start));
                assert_eq!(row[3], json!(want.end));
            }
            assert_eq!(got[0]["language"], json!(file.language));
            let notes = got[0]["notes"].to_string();
            assert_eq!(notes.contains("no indexer covered this file"), !file.covered, "{}: the note about coverage", file.path);
        }
    }
    // Covered but empty is not the same as not covered.
    let project = synthetic_project();
    let mut server = new_server(&project);
    let empty = server.call_tool("file_outline", &json!({"path": "src/empty.rs"}));
    let empty = check_response(&tool_def("file_outline"), 0, 6000, &empty).unwrap();
    assert_eq!((empty["covered"].as_bool(), empty["symbols"]["total"].as_i64()), (Some(true), Some(0)));
    assert!(empty.get("notes").is_none(), "a covered file with nothing in it has nothing to explain");
    let gap = server.call_tool("file_outline", &json!({"path": "src/uncovered.rs"}));
    let gap = check_response(&tool_def("file_outline"), 0, 6000, &gap).unwrap();
    assert_eq!((gap["covered"].as_bool(), gap["symbols"]["total"].as_i64()), (Some(false), Some(0)));
    assert!(gap["notes"].to_string().contains("no indexer covered this file"));
}

/// How `symbol_detail` orders a relation: kind of edge, count (most first), then name and id.
type RelationKey = (u8, i64, String, String);

fn relation_order(a: &RelationKey, b: &RelationKey) -> std::cmp::Ordering {
    (a.0, std::cmp::Reverse(a.1), &a.2, &a.3).cmp(&(b.0, std::cmp::Reverse(b.1), &b.2, &b.3))
}

fn symbol_detail_matches_database(project: &Project, max_nodes: usize) {
    let facts = Facts::load(project);
    let mut server = new_server(project);
    let def = tool_def("symbol_detail");
    let ids: Vec<&NodeFact> = facts.nodes.iter().filter(|n| addressable(&n.id)).collect();
    for node in sample(&ids, max_nodes) {
        let result = server.call_tool("symbol_detail", &json!({"id": node.id, "budget_tokens": MAX_BUDGET_TOKENS}));
        let a = check_response(&def, 0, MAX_BUDGET_TOKENS as usize * BYTES_PER_TOKEN, &result)
            .unwrap_or_else(|e| panic!("{}: {e}", node.id));
        let at = &node.id;
        assert_eq!(a["id"], json!(node.id), "{at}");
        assert_eq!(a["kind"], json!(node.kind), "{at}");
        assert_same(&a["name"], &json!(node.name), at);
        assert_same(&a["file"], &json!(node.file), at);
        assert_eq!(a["start"], json!(node.start), "{at}");
        assert_eq!(a["end"], json!(node.end), "{at}");
        assert_eq!(a["fan_in"], json!(node.fan_in), "{at}");
        assert_eq!(a["fan_out"], json!(node.fan_out), "{at}");
        assert_eq!(a["duplicate"], json!(node.duplicate), "{at}");
        assert_eq!(a["qualified"], if node.qualified.is_empty() { Value::Null } else { json!(node.qualified) }, "{at}");

        // The container is the smallest id among the nodes that contain or define this one.
        let container = facts
            .edges
            .iter()
            .filter(|e| e.target == node.id && (e.kind == "CONTAINS" || e.kind == "DEFINES"))
            .map(|e| e.source.as_str())
            .min();
        assert_same(&a["container"], &json!(container), at);

        // Members.
        let mut members: Vec<&NodeFact> = facts
            .edges
            .iter()
            .filter(|e| e.source == node.id && (e.kind == "CONTAINS" || e.kind == "DEFINES"))
            .map(|e| facts.node(&e.target))
            .collect();
        members.sort_by_key(|n| (n.start, std::cmp::Reverse(n.end), n.id.clone()));
        assert_eq!(a["members"]["total"], json!(members.len()), "{at}: members");
        for (row, want) in a["members"]["rows"].as_array().unwrap().iter().zip(&members) {
            assert_same(&row[4], &json!(want.id), at);
        }

        // What depends on it, and what it depends on.
        for (table, incoming) in [("dependents", true), ("dependencies", false)] {
            let mut related: Vec<(&EdgeFact, RelationKey)> = facts
                .edges
                .iter()
                .filter(|e| ["CALLS", "IMPLEMENTS", "REFERENCES", "USES", "DEPENDS_ON"].contains(&e.kind.as_str()))
                .filter(|e| if incoming { e.target == node.id } else { e.source == node.id })
                .map(|e| {
                    let o = facts.node(other(e, incoming));
                    (e, (priority(&e.kind), e.count, o.name.clone(), o.id.clone()))
                })
                .collect();
            related.sort_by(|a, b| relation_order(&a.1, &b.1));
            assert_eq!(a[table]["total"], json!(related.len()), "{at}: {table} total");
            for (row, (edge, _)) in a[table]["rows"].as_array().unwrap().iter().zip(&related) {
                let o = facts.node(other(edge, incoming));
                assert_eq!(row[0], json!(edge.kind), "{at}: {table} rel");
                assert_same(&row[1], &json!(o.id), at);
                assert_same(&row[4], &json!(o.file), &format!("{at}: {table} file"));
                assert_eq!(row[5], json!(o.start), "{at}: {table} line");
                assert_eq!(row[6], json!(edge.count), "{at}: {table} count");
                assert_eq!(row[7], json!(edge.lines.iter().take(5).collect::<Vec<_>>()), "{at}: {table} sites");
            }
        }
    }
}

#[test]
fn symbol_detail_agrees_with_the_database_on_real_indexes() {
    for name in REAL {
        symbol_detail_matches_database(&fixture_project(name), usize::MAX);
    }
}

#[test]
fn symbol_detail_agrees_with_the_database_at_scale() {
    symbol_detail_matches_database(&synthetic_project(), 80);
}

#[test]
fn read_source_returns_exactly_the_lines_of_the_file() {
    for project in [fixture_project("ts"), fixture_project("py"), fixture_project("rust"), synthetic_project()] {
        let facts = Facts::load(&project);
        let mut server = new_server(&project);
        let def = tool_def("read_source");
        let mut clipped_lines = 0;
        for file in facts.files.iter().filter(|f| f.lines > 0 && f.path.chars().count() <= mcp::PATH_MAX) {
            let source = facts.source(&file.path);
            assert_eq!(source.len() as i64, file.lines, "{}: the index and the file disagree about the line count", file.path);
            let last = file.lines.min(400);
            let mut ranges = vec![(1, last)];
            if file.lines >= 8 {
                ranges.push((3, 7));
                ranges.push((file.lines - 2, file.lines));
            }
            for (start, end) in ranges {
                let args = json!({"path": file.path, "start": start, "end": end});
                let got = small_pages(&mut server, &def, &args, &[700, 1_000, 1_600, 3_000]);
                let rows = rows_of(&got, "lines");
                assert_eq!(got[0]["start"], json!(start));
                assert_eq!(got[0]["end"], json!(end));
                assert_eq!(got[0]["lines_in_file"], json!(file.lines));
                assert_eq!(got[0]["lines"]["total"], json!(end - start + 1));
                assert_eq!(rows.len() as i64, end - start + 1, "{} {start}..{end}", file.path);
                for (offset, row) in rows.iter().enumerate() {
                    let number = start as usize + offset;
                    let want = format!("{number}: {}", source[number - 1]);
                    let line = row[0].as_str().unwrap();
                    if line == want {
                        continue;
                    }
                    // Only a line too long to show whole may differ, and it is cut, not changed.
                    let body = line.strip_suffix('…').unwrap_or_else(|| panic!("{} {number}: {line:?} != {want:?}", file.path));
                    assert!(want.starts_with(body), "{} {number}: {line:?} is not the start of {want:?}", file.path);
                    clipped_lines += 1;
                }
            }
        }
        if project.root.ends_with("fixture-ts") || project.root.ends_with("fixture-py") {
            assert!(clipped_lines > 0, "the 600-character line was never seen clipped");
        }
    }
}

#[test]
fn read_source_refuses_what_it_cannot_honestly_answer() {
    let project = fixture_project("py");
    let mut server = new_server(&project);
    let say = |server: &mut Server, args: Value| {
        let result = server.call_tool("read_source", &args);
        assert_eq!(result["isError"], json!(true), "{args}");
        text_of(&result).to_string()
    };
    assert!(say(&mut server, json!({"path": "pkg/__init__.py", "start": 1, "end": 1})).contains("past the end of the file (0 lines)"));
    assert!(say(&mut server, json!({"path": "pkg/shapes.py", "start": 999, "end": 1000})).contains("past the end of the file"));
    assert!(say(&mut server, json!({"path": "pkg/shapes.py", "start": 5, "end": 4})).contains("is before start"));
    assert!(say(&mut server, json!({"path": "pkg/shapes.py", "start": 1, "end": 401})).contains("at most 400"));
    assert!(say(&mut server, json!({"path": "pkg/shapes.pyy", "start": 1, "end": 2})).contains("is not an indexed file"));
    assert!(say(&mut server, json!({"path": "shapes.py", "start": 1, "end": 2})).contains("pkg/shapes.py"), "the hint names the real path");

    // A range that runs past the end is cut at the end, and says so.
    let a = server.call_tool("read_source", &json!({"path": "pkg/shapes.py", "start": 28, "end": 40}));
    let a = check_response(&tool_def("read_source"), 0, 6000, &a).unwrap();
    assert_eq!(a["end"], json!(30));
    assert!(a["notes"].to_string().contains("the file has 30 lines, so the range ends at 30"), "{}", a["notes"]);
    assert_eq!(a["lines"]["rows"].as_array().unwrap().len(), 3);

    // CRLF files come back without the carriage returns, and a last line without a newline is there.
    let a = server.call_tool("read_source", &json!({"path": "pkg/windows.py", "start": 1, "end": 2}));
    let a = check_response(&tool_def("read_source"), 0, 6000, &a).unwrap();
    assert_eq!(a["lines"]["rows"], json!([["1: def windows_line() -> str:"], ["2:     return \"crlf\""]]));
    let a = server.call_tool("read_source", &json!({"path": "pkg/nonewline.py", "start": 1, "end": 2}));
    let a = check_response(&tool_def("read_source"), 0, 6000, &a).unwrap();
    assert_eq!(a["lines"]["rows"][1], json!(["2:     return 1"]));
}

#[test]
fn overview_agrees_with_the_database() {
    for project in [fixture_project("ts"), fixture_project("py"), fixture_project("rust"), synthetic_project()] {
        let facts = Facts::load(&project);
        let mut server = new_server(&project);
        let def = tool_def("overview");
        let a = check_response(&def, 0, MAX_BUDGET_TOKENS as usize * BYTES_PER_TOKEN, &server.call_tool("overview", &json!({"budget_tokens": MAX_BUDGET_TOKENS}))).unwrap();
        assert_eq!(a["files"], json!(facts.files.len()));
        assert_eq!(a["covered"], json!(facts.files.iter().filter(|f| f.covered).count()));
        assert_eq!(a["nodes"], json!(facts.nodes.len()));
        assert_eq!(a["edges"], json!(facts.edges.len()));
        assert_eq!(a["long_ids"], json!(facts.nodes.iter().filter(|n| n.id.chars().count() > mcp::ID_MAX).count()));
        assert_eq!(a["fresh"], json!(true), "a project nobody touched is fresh");
        assert_eq!(a["changes"]["total"], json!(0));

        let mut kinds: std::collections::BTreeMap<&str, i64> = Default::default();
        for n in &facts.nodes {
            *kinds.entry(n.kind.as_str()).or_default() += 1;
        }
        let mut want: Vec<(&str, i64)> = kinds.into_iter().collect();
        want.sort_by_key(|(k, n)| (std::cmp::Reverse(*n), *k));
        let got: Vec<(&str, i64)> = a["kinds"]["rows"].as_array().unwrap().iter().map(|r| (r[0].as_str().unwrap(), r[1].as_i64().unwrap())).collect();
        assert_eq!(got, want, "node kinds");

        let mut edges: std::collections::BTreeMap<&str, i64> = Default::default();
        for e in &facts.edges {
            *edges.entry(e.kind.as_str()).or_default() += 1;
        }
        let mut want: Vec<(&str, i64)> = edges.into_iter().collect();
        want.sort_by_key(|(k, n)| (std::cmp::Reverse(*n), *k));
        let got: Vec<(&str, i64)> = a["edge_types"]["rows"].as_array().unwrap().iter().map(|r| (r[0].as_str().unwrap(), r[1].as_i64().unwrap())).collect();
        assert_eq!(got, want, "edge types");

        let uncovered: Vec<&str> = facts.files.iter().filter(|f| !f.covered).map(|f| f.path.as_str()).collect();
        let got: Vec<&str> = a["uncovered"]["rows"].as_array().unwrap().iter().map(|r| r[0].as_str().unwrap()).collect();
        assert_eq!(got.len(), uncovered.len().min(got.len()));
        assert_eq!(a["uncovered"]["total"], json!(uncovered.len()));
        for (g, w) in got.iter().zip(&uncovered) {
            assert_same(&json!(g), &json!(w), "uncovered");
        }
        if !uncovered.is_empty() {
            assert!(a["notes"].to_string().contains("have no semantic data"), "{}", a["notes"]);
        }
        let mut languages: std::collections::BTreeMap<&str, (i64, i64)> = Default::default();
        for f in &facts.files {
            let entry = languages.entry(f.language.as_str()).or_default();
            entry.0 += 1;
            entry.1 += f.covered as i64;
        }
        let got: Vec<(&str, i64, i64)> = a["languages"]["rows"].as_array().unwrap().iter().map(|r| (r[0].as_str().unwrap(), r[1].as_i64().unwrap(), r[2].as_i64().unwrap())).collect();
        let want: Vec<(&str, i64, i64)> = languages.into_iter().map(|(l, (n, c))| (l, n, c)).collect();
        assert_eq!(got, want, "languages");
    }
}

#[test]
fn limits_hold_at_scale_and_are_reported_not_hidden() {
    let project = synthetic_project();
    let facts = Facts::load(&project);
    let mut server = new_server(&project);
    let id = |descriptors: &str| format!("cargo:synth:{descriptors}");
    let call = |server: &mut Server, tool: &str, args: Value| {
        let def = tool_def(tool);
        let offset = args.get("offset").and_then(Value::as_i64).unwrap_or(0);
        let tokens = args.get("budget_tokens").and_then(Value::as_i64).unwrap_or(DEFAULT_BUDGET_TOKENS);
        check_response(&def, offset, tokens as usize * BYTES_PER_TOKEN, &server.call_tool(tool, &args)).unwrap_or_else(|e| panic!("{tool} {args}: {e}"))
    };

    // A hub with thousands of callers: the total is the truth, the table is what fits, and the
    // answer says it is partial.
    let callers = facts.edges.iter().filter(|e| e.target == id("hub/hub_a().") && e.kind == "CALLS").count() as i64;
    assert!(callers > 6500);
    let hub = call(&mut server, "symbol_detail", json!({"id": id("hub/hub_a()."), "budget_tokens": MAX_BUDGET_TOKENS}));
    assert_eq!(hub["dependents"]["total"], json!(callers));
    let shown = hub["dependents"]["shown"].as_i64().unwrap();
    assert!(0 < shown && shown < 300, "{shown}");
    assert!(hub["notes"].to_string().contains(&format!("`dependents` shows {shown} of {callers} rows")), "{}", hub["notes"]);

    // The row cap, not the byte budget, ends this page: 1,000 one-character lines.
    let page = call(&mut server, "read_source", json!({"path": "src/lines.rs", "start": 1, "end": 400, "budget_tokens": MAX_BUDGET_TOKENS}));
    assert_eq!((page["lines"]["shown"].as_i64(), page["lines"]["next_offset"].as_i64()), (Some(300), Some(300)));
    let rest = call(&mut server, "read_source", json!({"path": "src/lines.rs", "start": 1, "end": 400, "offset": 300, "budget_tokens": MAX_BUDGET_TOKENS}));
    assert_eq!((rest["lines"]["shown"].as_i64(), rest["lines"]["next_offset"].clone()), (Some(100), Value::Null));
    assert_eq!(rest["lines"]["rows"][0], json!(["301: x"]));

    // An id too long to show whole, a name near the limit, a path too long to pass back.
    let long = call(&mut server, "find_symbols", json!({"query": "l".repeat(150)}));
    assert_eq!(long["symbols"]["total"], json!(1));
    let shown_id = long["symbols"]["rows"][0][0].as_str().unwrap();
    assert!(shown_id.ends_with('…') && serde_json::to_string(shown_id).unwrap().len() <= mcp::ID_CLIP);
    assert!(long["clipped"].as_i64().unwrap() >= 1 && long["notes"].to_string().contains("a clipped id cannot be passed to other tools"));
    let overview = call(&mut server, "overview", json!({}));
    assert_eq!(overview["long_ids"], json!(1));
    let mid = call(&mut server, "find_symbols", json!({"query": "m".repeat(150)}));
    let mid_id = mid["symbols"]["rows"][0][0].as_str().unwrap().to_string();
    assert!(mid_id.chars().count() > 512 && !mid_id.ends_with('…'), "an id past the old limit still comes back whole");
    let detail = call(&mut server, "symbol_detail", json!({"id": mid_id}));
    assert_eq!(detail["id"], json!(mid_id));

    let deep = call(&mut server, "find_symbols", json!({"query": "deep_fn"}));
    assert!(deep["symbols"]["rows"][0][3].as_str().unwrap().ends_with('…'));
    assert!(deep["notes"].to_string().contains("clipped"));
    let too_long = facts.files.iter().find(|f| f.path.ends_with("deep.rs")).unwrap();
    let refused = server.call_tool("file_outline", &json!({"path": too_long.path}));
    assert!(refused["isError"] == json!(true) && text_of(&refused).contains("$.path: length"), "{}", text_of(&refused));
    let near = facts.files.iter().find(|f| f.path.ends_with("near.rs")).unwrap();
    assert!(near.path.chars().count() > 400, "this path would have been refused under the old limit");
    let outline = call(&mut server, "file_outline", json!({"path": near.path}));
    assert_eq!(outline["symbols"]["rows"][0][1], json!("near_fn"));

    // Names that need escaping, a duplicate definition, a non-ASCII path, and ranking.
    let weird = call(&mut server, "find_symbols", json!({"query": "we\"ird"}));
    assert_eq!(weird["symbols"]["rows"][0][2], json!("we\"ird\\name"));
    let weird_id = weird["symbols"]["rows"][0][0].as_str().unwrap();
    assert!(weird_id.contains('"') && weird_id.contains('\\'));
    assert_eq!(call(&mut server, "symbol_detail", json!({"id": weird_id}))["name"], json!("we\"ird\\name"));
    let dup = call(&mut server, "find_symbols", json!({"query": "dup_fn"}));
    let ids: Vec<&str> = dup["symbols"]["rows"].as_array().unwrap().iter().map(|r| r[0].as_str().unwrap()).collect();
    assert_eq!(ids, vec!["cargo:synth:dup/dup_fn().", "cargo:synth:dup/dup_fn().~2"]);
    assert_eq!(call(&mut server, "symbol_detail", json!({"id": ids[1]}))["duplicate"], json!(true));
    assert_eq!(call(&mut server, "symbol_detail", json!({"id": ids[0]}))["duplicate"], json!(false));
    let unicode = call(&mut server, "file_outline", json!({"path": "src/ünï/cödé.rs"}));
    assert_eq!(unicode["symbols"]["rows"][0][1], json!("grüße"));
    assert_eq!(call(&mut server, "read_source", json!({"path": "src/ünï/cödé.rs", "start": 2, "end": 2}))["lines"]["rows"][0], json!(["2: pub fn grüße() {"]));
    let ranked = call(&mut server, "find_symbols", json!({"query": "render"}));
    let names: Vec<&str> = ranked["symbols"]["rows"].as_array().unwrap().iter().map(|r| r[2].as_str().unwrap()).collect();
    // Exact names first (the one other code calls leads, then by name: capitals sort first), then
    // names that start with it, then names that merely contain it.
    assert_eq!(names, vec!["render", "Render", "render", "RENDER_TWICE", "renderer", "prerender", "unrender_all"]);
}

// ------------------------------------------------------------------------------------------
// Freshness.
// ------------------------------------------------------------------------------------------

#[test]
fn answers_say_when_a_file_changed_after_indexing_and_only_for_that_file() {
    let project = fixture_project("ts");
    let mut server = new_server(&project);
    let facts = Facts::load(&project);
    let chart = facts.nodes.iter().find(|n| n.name == "Chart" && n.kind == "TYPE").unwrap().id.clone();

    let stale = |server: &mut Server, tool: &str, args: Value| -> Value {
        let result = server.call_tool(tool, &args);
        assert_eq!(result["isError"], json!(false), "{tool} {args}: {}", text_of(&result));
        serde_json::from_str(text_of(&result)).unwrap()
    };
    for (tool, args) in [
        ("file_outline", json!({"path": "src/shapes.ts"})),
        ("find_symbols", json!({"query": "Chart"})),
        ("symbol_detail", json!({"id": chart})),
        ("trace", json!({"id": chart, "direction": "dependents"})),
        ("read_source", json!({"path": "src/shapes.ts", "start": 1, "end": 3})),
        ("overview", json!({})),
    ] {
        assert!(stale(&mut server, tool, args)["stale_files"].is_null(), "{tool}: nothing has changed yet");
    }

    let path = project.root.join("src/shapes.ts");
    let original = fs::read_to_string(&path).unwrap();
    fs::write(&path, format!("// added after indexing\n{original}")).unwrap();
    for (tool, args) in [
        ("file_outline", json!({"path": "src/shapes.ts"})),
        ("find_symbols", json!({"query": "Chart"})),
        ("symbol_detail", json!({"id": chart})),
        ("trace", json!({"id": chart, "direction": "dependents"})),
        ("read_source", json!({"path": "src/shapes.ts", "start": 1, "end": 3})),
    ] {
        let a = stale(&mut server, tool, args.clone());
        assert_eq!(a["stale_files"], json!(["src/shapes.ts"]), "{tool} {args}");
        assert!(a["notes"][0].as_str().unwrap().contains("changed"), "{tool}: {}", a["notes"]);
    }
    // Other files are unaffected, and the read shows what is on disk now.
    assert!(stale(&mut server, "file_outline", json!({"path": "src/app.ts"}))["stale_files"].is_null());
    let read = stale(&mut server, "read_source", json!({"path": "src/shapes.ts", "start": 1, "end": 1}));
    assert_eq!(read["lines"]["rows"][0], json!(["1: // added after indexing"]));
    let overview = stale(&mut server, "overview", json!({}));
    assert_eq!(overview["fresh"], json!(false));
    assert_eq!(overview["changes"]["rows"], json!([["src/shapes.ts", "changed"]]));

    // A deleted file and a new file.
    fs::remove_file(project.root.join("src/util/log.ts")).unwrap();
    fs::write(project.root.join("src/new.ts"), "export const added = 1;\n").unwrap();
    let outline = stale(&mut server, "file_outline", json!({"path": "src/util/log.ts"}));
    assert_eq!(outline["stale_files"], json!(["src/util/log.ts"]));
    assert!(outline["notes"][0].as_str().unwrap().contains("1 missing"));
    let gone = server.call_tool("read_source", &json!({"path": "src/util/log.ts", "start": 1, "end": 1}));
    assert!(gone["isError"] == json!(true) && text_of(&gone).contains("cannot be read"));
    let overview = stale(&mut server, "overview", json!({}));
    let changes: Vec<(String, String)> = overview["changes"]["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r[0].as_str().unwrap().to_string(), r[1].as_str().unwrap().to_string()))
        .collect();
    assert_eq!(
        changes,
        vec![
            ("src/new.ts".to_string(), "new".to_string()),
            ("src/shapes.ts".to_string(), "changed".to_string()),
            ("src/util/log.ts".to_string(), "missing".to_string()),
        ]
    );

    // Putting the content back makes it fresh again, with no restart.
    fs::write(&path, original).unwrap();
    fs::write(project.root.join("src/util/log.ts"), fs::read_to_string(fixtures_dir().join("ts/src/util/log.ts")).unwrap()).unwrap();
    fs::remove_file(project.root.join("src/new.ts")).unwrap();
    assert_eq!(stale(&mut server, "overview", json!({}))["fresh"], json!(true));

    // Indexing again takes the change in: the new file is now known, and nothing is stale.
    fs::write(project.root.join("src/new.ts"), "export const added = 1;\n").unwrap();
    index_from_scip(&project.root, &fixtures_dir().join("scip/ts.scip"));
    let overview = stale(&mut server, "overview", json!({}));
    assert_eq!(overview["fresh"], json!(true));
    assert_eq!(overview["files"], json!(13), "the server reads the rebuilt index without a restart");
}

// ------------------------------------------------------------------------------------------
// The checkers must catch cheating, or none of the above means anything.
// ------------------------------------------------------------------------------------------

fn symbols_of(answer: &mut Value) -> &mut Value {
    &mut answer["symbols"]
}

fn rewrap(result: &Value, mutate: impl FnOnce(&mut Value)) -> Value {
    let mut answer: Value = serde_json::from_str(text_of(result)).unwrap();
    mutate(&mut answer);
    json!({"content": [{"type": "text", "text": answer.to_string()}], "structuredContent": answer, "isError": false})
}

#[test]
fn the_checks_reject_every_way_an_answer_could_cheat() {
    let project = fixture_project("ts");
    let mut server = new_server(&project);

    // A paginated answer with more to come.
    let def = tool_def("find_symbols");
    let args = json!({"query": "e", "offset": 0});
    let page = server.call_tool_with_budget_bytes("find_symbols", &args, 900);
    let honest = check_response(&def, 0, 900, &page).expect("the honest answer passes");
    assert!(honest["symbols"]["next_offset"].is_i64(), "the sample needs a continuation");

    let cheats: Vec<(&str, Value)> = vec![
        ("shown larger than the rows", rewrap(&page, |a| symbols_of(a)["shown"] = json!(symbols_of(a)["shown"].as_i64().unwrap() + 1))),
        ("a row removed, shown unchanged", rewrap(&page, |a| { symbols_of(a)["rows"].as_array_mut().unwrap().pop(); })),
        ("total below what is shown", rewrap(&page, |a| symbols_of(a)["total"] = json!(1))),
        ("no continuation although rows remain", rewrap(&page, |a| symbols_of(a)["next_offset"] = Value::Null)),
        ("a continuation that skips rows", rewrap(&page, |a| symbols_of(a)["next_offset"] = json!(symbols_of(a)["shown"].as_i64().unwrap() + 3))),
        ("a continuation after nothing was shown", rewrap(&page, |a| { symbols_of(a)["rows"] = json!([]); symbols_of(a)["shown"] = json!(0); symbols_of(a)["next_offset"] = json!(0); })),
        ("columns renamed", rewrap(&page, |a| symbols_of(a)["columns"][1] = json!("type"))),
        ("a cell of the wrong type", rewrap(&page, |a| symbols_of(a)["rows"][0][4] = json!("not a line number"))),
        ("a row too narrow", rewrap(&page, |a| symbols_of(a)["rows"][0] = json!(["only one cell"]))),
        ("an undeclared field", rewrap(&page, |a| a["extra"] = json!(1))),
        ("as_of missing", rewrap(&page, |a| { a.as_object_mut().unwrap().remove("as_of"); })),
        ("as_of empty", rewrap(&page, |a| a["as_of"] = json!(""))),
        ("past the budget", rewrap(&page, |a| a["notes"] = json!(["x".repeat(2000)]))),
        ("an error result", json!({"content": [{"type": "text", "text": "boom"}], "isError": true})),
        ("text that is not JSON", json!({"content": [{"type": "text", "text": "{nope"}], "isError": false})),
        ("structured content differing from the text", {
            let mut r = page.clone();
            r["structuredContent"] = json!({"as_of": "x"});
            r
        }),
        ("no structured content", {
            let mut r = page.clone();
            r.as_object_mut().unwrap().remove("structuredContent");
            r
        }),
    ];
    for (what, cheat) in &cheats {
        assert!(check_response(&def, 0, 900, cheat).is_err(), "an answer with {what} was accepted");
    }

    // A table that is not paginated and was cut without saying so.
    let overview = tool_def("overview");
    let mut found = None;
    for bytes in (1_000..1_400).step_by(10) {
        let result = server.call_tool_with_budget_bytes("overview", &json!({}), bytes);
        if result["isError"] == json!(false) {
            let a: Value = serde_json::from_str(text_of(&result)).unwrap();
            if overview.tables.iter().any(|t| a[t.name]["shown"].as_i64() < a[t.name]["total"].as_i64()) {
                found = Some((bytes, result));
                break;
            }
        }
    }
    let (bytes, result) = found.expect("some budget cuts a table of the overview");
    assert!(check_response(&overview, 0, bytes, &result).is_ok());
    assert!(check_response(&overview, 0, bytes, &rewrap(&result, |a| { a.as_object_mut().unwrap().remove("notes"); })).is_err(), "a cut table without a note was accepted");
    assert!(check_response(&overview, 0, bytes, &rewrap(&result, |a| a["kinds"]["next_offset"] = json!(5))).is_err());
}

#[test]
fn tools_cannot_write_and_the_source_says_so() {
    // The index is opened read-only, which the framework tests prove. This is a second, cruder
    // tripwire: nobody adds a statement that changes the database to the tools without a test
    // failing and someone having to read this comment.
    let source = include_str!("tools.rs");
    let source = source.split("#[cfg(test)]").next().unwrap();
    for forbidden in [
        "INSERT INTO", "DELETE FROM", "UPDATE ", "DROP TABLE", "CREATE TABLE", "ALTER TABLE", "ATTACH", "PRAGMA ",
        "VACUUM", "REPLACE INTO", ".execute(", ".execute_batch(", "fs::write", "fs::remove", "fs::create", "File::create",
    ] {
        assert!(!source.contains(forbidden), "tools.rs contains {forbidden:?}");
    }
}

#[test]
fn nothing_in_the_whole_suite_of_calls_changes_the_project_or_its_index() {
    for project in [fixture_project("rust"), synthetic_project()] {
        let facts = Facts::load(&project);
        let before = digest(&project.root);
        let mut server = new_server(&project);
        for def in tools::registry() {
            for args in sample(&cases(def.name, &facts), 40) {
                let _ = server.call_tool(def.name, &args);
            }
        }
        assert_eq!(digest(&project.root), before);
    }
}

