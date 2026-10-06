//! Builds the code graph from SCIP indexes.
//!
//! SCIP indexes come from compiler-grade indexers (rust-analyzer, scip-typescript,
//! scip-python, ...), so every reference here is already *resolved*: a call to `render()`
//! points at `Chart#render()` or `Table#render()`, never at "something called render".
//! This module only reshapes that data into nodes and edges.
//!
//! Node types: FILE, MODULE (a directory), EXTERNAL (a dependency package), plus the symbol
//! types in `symbols::SYMBOL_TYPES`.
//!
//! Edge types:
//! * `CONTAINS`   directory -> file, type -> member
//! * `DEFINES`    file -> top-level symbol
//! * `CALLS`      symbol -> function/method, for a reference made inside another definition
//! * `REFERENCES` any other resolved reference (types, fields, imports, module-level code)
//! * `IMPLEMENTS` symbol -> symbol, from the indexer's relationship data
//! * `USES`       symbol/file -> EXTERNAL package
//! * `DEPENDS_ON` file -> file and directory -> directory, derived from the edges above

use crate::scanner::SourceFile;
use crate::symbols::{self, Entity};
use scip::types::occurrence::{Typed_enclosing_range, Typed_range};
use scip::types::symbol_information::Kind;
use scip::types::{Document, Index, Occurrence, SymbolInformation};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

const ROLE_DEFINITION: i32 = 1;
const SAMPLE_LINES: usize = 5;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Node {
    pub id: String,
    #[serde(rename = "type")]
    pub node_type: String,
    pub name: String,
    pub file_path: Option<String>,
    pub start_line: Option<i64>,
    pub end_line: Option<i64>,
    pub language: Option<String>,
    pub metadata: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Edge {
    pub source_id: String,
    pub target_id: String,
    #[serde(rename = "type")]
    pub edge_type: String,
    pub metadata: Value,
}

#[derive(Debug, Default, Clone)]
pub struct Graph {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
}

/// One SCIP index to ingest.
pub struct IndexInput<'a> {
    pub index: &'a Index,
    /// Where the indexer ran, relative to the project root ("" if it ran at the root).
    /// Document paths in the index are relative to that directory.
    pub root_prefix: String,
}

#[derive(Debug, Clone)]
pub struct FileRecord {
    pub file: SourceFile,
    /// The indexer that produced semantic data for the file, `None` if nothing covered it.
    pub indexed_by: Option<String>,
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct BuildStats {
    pub documents: usize,
    pub documents_ignored: usize,
    pub documents_duplicate: usize,
    pub definitions: usize,
    pub duplicate_definitions: usize,
    pub references_resolved: usize,
    pub references_external: usize,
    /// References to symbols of the project's own packages that no ingested file defines
    /// (generated code, ignored files). They produce no edge.
    pub references_unresolved: usize,
}

pub struct BuildOutput {
    pub graph: Graph,
    pub files: Vec<FileRecord>,
    pub stats: BuildStats,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Pos {
    line: i32,
    col: i32,
}

#[derive(Clone, Copy, Debug)]
struct Span {
    start: Pos,
    end: Pos,
}

fn span_from(raw: &[i32]) -> Option<Span> {
    match raw {
        [line, start, end] => Some(Span {
            start: Pos { line: *line, col: *start },
            end: Pos { line: *line, col: *end },
        }),
        [start_line, start, end_line, end] => Some(Span {
            start: Pos { line: *start_line, col: *start },
            end: Pos { line: *end_line, col: *end },
        }),
        _ => None,
    }
}

fn occurrence_span(o: &Occurrence) -> Option<Span> {
    span_from(&o.range).or_else(|| match &o.typed_range {
        Some(Typed_range::SingleLineRange(r)) => Some(Span {
            start: Pos { line: r.line, col: r.start_character },
            end: Pos { line: r.line, col: r.end_character },
        }),
        Some(Typed_range::MultiLineRange(r)) => Some(Span {
            start: Pos { line: r.start_line, col: r.start_character },
            end: Pos { line: r.end_line, col: r.end_character },
        }),
        _ => None,
    })
}

fn enclosing_span(o: &Occurrence) -> Option<Span> {
    span_from(&o.enclosing_range).or_else(|| match &o.typed_enclosing_range {
        Some(Typed_enclosing_range::SingleLineEnclosingRange(r)) => Some(Span {
            start: Pos { line: r.line, col: r.start_character },
            end: Pos { line: r.line, col: r.end_character },
        }),
        Some(Typed_enclosing_range::MultiLineEnclosingRange(r)) => Some(Span {
            start: Pos { line: r.start_line, col: r.start_character },
            end: Pos { line: r.end_line, col: r.end_character },
        }),
        _ => None,
    })
}

/// SCIP lines are 0-based and a range that ends in column 0 stops at the end of the
/// previous line, so `1:0-5:0` covers lines 2..=5 (1-based).
fn start_line(span: &Span) -> i64 {
    span.start.line as i64 + 1
}

fn end_line(span: &Span) -> i64 {
    if span.end.col == 0 && span.end.line > span.start.line {
        span.end.line as i64
    } else {
        span.end.line as i64 + 1
    }
}

/// A short label for the tool that wrote an index, e.g. `rust-analyzer 1.97.0`.
pub fn indexer_label(index: &Index) -> String {
    let tool = &index.metadata.tool_info;
    match (tool.name.is_empty(), tool.version.is_empty()) {
        (true, _) => "scip".to_string(),
        (false, true) => tool.name.clone(),
        (false, false) => format!("{} {}", tool.name, tool.version),
    }
}

fn join_path(prefix: &str, relative: &str) -> String {
    let relative = relative.trim_start_matches("./");
    let prefix = prefix.trim_matches('/');
    if prefix.is_empty() || prefix == "." {
        relative.to_string()
    } else {
        format!("{prefix}/{relative}")
    }
}

fn dir_of(path: &str) -> &str {
    path.rsplit_once('/').map_or(".", |(dir, _)| dir)
}

fn file_name_of(path: &str) -> &str {
    path.rsplit_once('/').map_or(path, |(_, name)| name)
}

/// Interned analysis of every distinct symbol string, so a million references to a
/// few thousand symbols are parsed a few thousand times.
#[derive(Default)]
struct SymTable {
    by_raw: HashMap<String, usize>,
    items: Vec<symbols::Parsed>,
}

impl SymTable {
    fn intern(&mut self, raw: &str) -> usize {
        if let Some(&i) = self.by_raw.get(raw) {
            return i;
        }
        let i = self.items.len();
        self.items.push(symbols::analyze(raw, None));
        self.by_raw.insert(raw.to_string(), i);
        i
    }
}

struct Def {
    node_id: String,
    parent_id: Option<String>,
    span: Option<Span>,
    /// Ids of the symbols this one implements or overrides.
    implements: Vec<String>,
}

struct Reference {
    sym: usize,
    pos: Pos,
}

struct Doc {
    file_id: String,
    defs: Vec<Def>,
    refs: Vec<Reference>,
}

#[derive(Default)]
struct Agg {
    count: i64,
    lines: Vec<i64>,
}

impl Agg {
    fn add(&mut self, line: i64) {
        self.count += 1;
        if self.lines.len() < SAMPLE_LINES && !self.lines.contains(&line) {
            self.lines.push(line);
        }
    }
}

#[derive(Default)]
struct Builder {
    stats: BuildStats,
    nodes: Vec<Node>,
    node_index: HashMap<String, usize>,
    definition_counts: HashMap<String, usize>,
    project_packages: HashSet<(String, String)>,
    syms: SymTable,
    docs: Vec<Doc>,
    covered: BTreeMap<String, FileRecord>,
}

impl Builder {
    fn push_node(&mut self, node: Node) {
        self.node_index.insert(node.id.clone(), self.nodes.len());
        self.nodes.push(node);
    }

    fn add_document(&mut self, label: &str, document: &Document, file: SourceFile) {
        let file_id = format!("file:{}", file.path);
        self.push_node(Node {
            id: file_id.clone(),
            node_type: "FILE".to_string(),
            name: file_name_of(&file.path).to_string(),
            file_path: Some(file.path.clone()),
            start_line: Some(1),
            end_line: Some(file.lines.max(1)),
            language: Some(file.language.clone()),
            metadata: json!({
                "hash": file.hash, "size": file.size, "lines": file.lines,
                "covered": true, "indexer": label
            }),
        });

        let infos: HashMap<&str, &SymbolInformation> = document
            .symbols
            .iter()
            .map(|s| (s.symbol.as_str(), s))
            .collect();
        let mut doc = Doc {
            file_id,
            defs: Vec::new(),
            refs: Vec::new(),
        };

        for occurrence in &document.occurrences {
            let Some(range) = occurrence_span(occurrence) else {
                continue;
            };
            if occurrence.symbol_roles & ROLE_DEFINITION != 0 {
                let info = infos.get(occurrence.symbol.as_str()).copied();
                self.add_definition(label, &file, occurrence, range, info, &mut doc);
            } else if !occurrence.symbol.is_empty() {
                // Parameters, locals and module path qualifiers are not graph entities.
                let sym = self.syms.intern(&occurrence.symbol);
                if matches!(self.syms.items[sym].entity, Entity::Symbol(_)) {
                    doc.refs.push(Reference { sym, pos: range.start });
                }
            }
        }

        self.covered.insert(
            file.path.clone(),
            FileRecord {
                file,
                indexed_by: Some(label.to_string()),
            },
        );
        self.docs.push(doc);
    }

    fn add_definition(
        &mut self,
        label: &str,
        file: &SourceFile,
        occurrence: &Occurrence,
        range: Span,
        info: Option<&SymbolInformation>,
        doc: &mut Doc,
    ) {
        let parsed = symbols::analyze(&occurrence.symbol, info);
        let node_type = match parsed.entity {
            Entity::Skip => return,
            // A module that starts at the top of the file is the file itself.
            Entity::Module if range.start == (Pos { line: 0, col: 0 }) => return,
            Entity::Module => "NAMESPACE",
            Entity::Symbol(node_type) => node_type,
        };

        // The same symbol can be defined more than once (two binaries that both have
        // `main`, overloads). The first site, in path order, is the canonical node.
        let seen = self.definition_counts.entry(parsed.id.clone()).or_insert(0);
        *seen += 1;
        let duplicate = *seen > 1;
        let node_id = if duplicate {
            format!("{}~{}", parsed.id, seen)
        } else {
            parsed.id.clone()
        };

        let span = enclosing_span(occurrence).unwrap_or(range);
        let mut metadata = serde_json::Map::new();
        metadata.insert("symbol".into(), json!(occurrence.symbol));
        if !parsed.qualified.is_empty() {
            metadata.insert("qualified".into(), json!(parsed.qualified));
        }
        if let Some((manager, name)) = &parsed.package {
            metadata.insert("package".into(), json!(format!("{manager}/{name}")));
        }
        metadata.insert("indexer".into(), json!(label));
        let mut implements = Vec::new();
        if let Some(info) = info {
            if let Ok(kind) = info.kind.enum_value() {
                if kind != Kind::UnspecifiedKind {
                    metadata.insert("kind".into(), json!(format!("{kind:?}")));
                }
            }
            if let Some(signature) = symbols::signature_of(info) {
                metadata.insert("signature".into(), json!(signature));
            }
            if let Some(documentation) = symbols::doc_of(info) {
                metadata.insert("doc".into(), json!(documentation));
            }
            for relationship in &info.relationships {
                if relationship.is_implementation {
                    let target = symbols::analyze(&relationship.symbol, None);
                    if target.entity != Entity::Skip && target.id != parsed.id {
                        implements.push(target.id);
                    }
                }
            }
        }
        if duplicate {
            metadata.insert("duplicateOf".into(), json!(parsed.id));
            self.stats.duplicate_definitions += 1;
        }
        if let Some(package) = &parsed.package {
            self.project_packages.insert(package.clone());
        }

        self.stats.definitions += 1;
        self.push_node(Node {
            id: node_id.clone(),
            node_type: node_type.to_string(),
            name: parsed.name,
            file_path: Some(file.path.clone()),
            start_line: Some(start_line(&span)),
            end_line: Some(end_line(&span)),
            language: Some(file.language.clone()),
            metadata: Value::Object(metadata),
        });
        doc.defs.push(Def {
            node_id,
            parent_id: parsed.parent_id,
            span: enclosing_span(occurrence),
            implements,
        });
    }

    fn finish(mut self, scanned: &[SourceFile]) -> BuildOutput {
        // Files nothing covered still get a node, flagged, so gaps are visible in the graph.
        for file in scanned {
            if self.covered.contains_key(&file.path) {
                continue;
            }
            self.push_node(Node {
                id: format!("file:{}", file.path),
                node_type: "FILE".to_string(),
                name: file_name_of(&file.path).to_string(),
                file_path: Some(file.path.clone()),
                start_line: Some(1),
                end_line: Some(file.lines.max(1)),
                language: Some(file.language.clone()),
                metadata: json!({
                    "hash": file.hash, "size": file.size, "lines": file.lines, "covered": false
                }),
            });
            self.covered.insert(
                file.path.clone(),
                FileRecord {
                    file: file.clone(),
                    indexed_by: None,
                },
            );
        }

        let mut edges: Vec<Edge> = Vec::new();
        let edge = |source: &str, target: &str, kind: &str, metadata: Value| Edge {
            source_id: source.to_string(),
            target_id: target.to_string(),
            edge_type: kind.to_string(),
            metadata,
        };

        // Directories.
        let mut dir_files: BTreeMap<String, usize> = BTreeMap::new();
        for path in self.covered.keys() {
            *dir_files.entry(dir_of(path).to_string()).or_default() += 1;
        }
        for (dir, files) in &dir_files {
            self.push_node(Node {
                id: format!("module:{dir}"),
                node_type: "MODULE".to_string(),
                name: dir.clone(),
                file_path: None,
                start_line: None,
                end_line: None,
                language: None,
                metadata: json!({ "path": dir, "files": files }),
            });
        }
        for path in self.covered.keys() {
            edges.push(edge(
                &format!("module:{}", dir_of(path)),
                &format!("file:{path}"),
                "CONTAINS",
                json!({}),
            ));
        }

        let docs = std::mem::take(&mut self.docs);

        // Structure: members hang off their owner type, everything else off its file.
        for doc in &docs {
            for def in &doc.defs {
                let owner = def
                    .parent_id
                    .as_ref()
                    .filter(|p| **p != def.node_id && self.node_index.contains_key(*p));
                match owner {
                    Some(owner) => edges.push(edge(owner, &def.node_id, "CONTAINS", json!({}))),
                    None => edges.push(edge(&doc.file_id, &def.node_id, "DEFINES", json!({}))),
                }
                for target in &def.implements {
                    if self.node_index.contains_key(target) {
                        edges.push(edge(&def.node_id, target, "IMPLEMENTS", json!({})));
                    }
                }
            }
        }

        // References.
        let mut aggregated: BTreeMap<(String, String, &'static str), Agg> = BTreeMap::new();
        let mut externals: BTreeSet<(String, String)> = BTreeSet::new();
        for doc in &docs {
            let mut intervals: Vec<(Span, &str)> = doc
                .defs
                .iter()
                .filter_map(|d| d.span.map(|s| (s, d.node_id.as_str())))
                .collect();
            intervals.sort_by(|a, b| a.0.start.cmp(&b.0.start).then(b.0.end.cmp(&a.0.end)));
            let mut refs: Vec<&Reference> = doc.refs.iter().collect();
            refs.sort_by_key(|r| r.pos);

            let mut open: Vec<usize> = Vec::new();
            let mut next = 0;
            for reference in refs {
                while next < intervals.len() && intervals[next].0.start <= reference.pos {
                    while open.last().is_some_and(|&t| intervals[t].0.end <= intervals[next].0.start)
                    {
                        open.pop();
                    }
                    open.push(next);
                    next += 1;
                }
                while open.last().is_some_and(|&t| intervals[t].0.end <= reference.pos) {
                    open.pop();
                }
                let from_definition = open.last().map(|&t| intervals[t].1);
                let source = from_definition.unwrap_or(doc.file_id.as_str());

                let target = &self.syms.items[reference.sym];
                let line = reference.pos.line as i64 + 1;
                if let Some(&idx) = self.node_index.get(&target.id) {
                    // Only a reference made from inside another definition counts as a call;
                    // at module level an import and a call are indistinguishable in SCIP.
                    let kind = if from_definition.is_some()
                        && symbols::is_callable(&self.nodes[idx].node_type)
                    {
                        "CALLS"
                    } else {
                        "REFERENCES"
                    };
                    aggregated
                        .entry((source.to_string(), target.id.clone(), kind))
                        .or_default()
                        .add(line);
                    self.stats.references_resolved += 1;
                } else if let Some(package) = &target.package {
                    if self.project_packages.contains(package) {
                        self.stats.references_unresolved += 1;
                    } else {
                        externals.insert(package.clone());
                        let external = format!("external:{}:{}", package.0, package.1);
                        aggregated
                            .entry((source.to_string(), external, "USES"))
                            .or_default()
                            .add(line);
                        self.stats.references_external += 1;
                    }
                }
            }
        }

        for (manager, name) in &externals {
            self.push_node(Node {
                id: format!("external:{manager}:{name}"),
                node_type: "EXTERNAL".to_string(),
                name: name.clone(),
                file_path: None,
                start_line: None,
                end_line: None,
                language: None,
                metadata: json!({ "manager": manager, "package": name }),
            });
        }
        for ((source, target, kind), agg) in &aggregated {
            edges.push(edge(
                source,
                target,
                kind,
                json!({ "count": agg.count, "lines": agg.lines }),
            ));
        }

        // File and directory dependencies, derived from the resolved edges only.
        let file_of: HashMap<&str, &str> = self
            .nodes
            .iter()
            .filter_map(|n| n.file_path.as_deref().map(|f| (n.id.as_str(), f)))
            .collect();
        let mut file_deps: BTreeMap<(String, String), i64> = BTreeMap::new();
        let mut dir_deps: BTreeMap<(String, String), i64> = BTreeMap::new();
        for ((source, target, kind), agg) in &aggregated {
            if !matches!(*kind, "CALLS" | "REFERENCES") {
                continue;
            }
            let (Some(from), Some(to)) = (file_of.get(source.as_str()), file_of.get(target.as_str()))
            else {
                continue;
            };
            if from == to {
                continue;
            }
            *file_deps.entry((from.to_string(), to.to_string())).or_default() += agg.count;
            let (from_dir, to_dir) = (dir_of(from), dir_of(to));
            if from_dir != to_dir {
                *dir_deps
                    .entry((from_dir.to_string(), to_dir.to_string()))
                    .or_default() += agg.count;
            }
        }
        for ((from, to), count) in &file_deps {
            edges.push(edge(
                &format!("file:{from}"),
                &format!("file:{to}"),
                "DEPENDS_ON",
                json!({ "count": count }),
            ));
        }
        for ((from, to), count) in &dir_deps {
            edges.push(edge(
                &format!("module:{from}"),
                &format!("module:{to}"),
                "DEPENDS_ON",
                json!({ "count": count }),
            ));
        }

        // Hubs: how many distinct nodes depend on each node, and on how many it depends.
        let mut fan_in: HashMap<&str, HashSet<&str>> = HashMap::new();
        let mut fan_out: HashMap<&str, HashSet<&str>> = HashMap::new();
        for e in &edges {
            if matches!(
                e.edge_type.as_str(),
                "CALLS" | "REFERENCES" | "IMPLEMENTS" | "USES" | "DEPENDS_ON"
            ) && e.source_id != e.target_id
            {
                fan_in.entry(&e.target_id).or_default().insert(&e.source_id);
                fan_out.entry(&e.source_id).or_default().insert(&e.target_id);
            }
        }
        let scores: Vec<(usize, usize)> = self
            .nodes
            .iter()
            .map(|n| {
                (
                    fan_in.get(n.id.as_str()).map_or(0, |s| s.len()),
                    fan_out.get(n.id.as_str()).map_or(0, |s| s.len()),
                )
            })
            .collect();
        for (node, (fan_in, fan_out)) in self.nodes.iter_mut().zip(scores) {
            if let Value::Object(metadata) = &mut node.metadata {
                metadata.insert("fanIn".into(), json!(fan_in));
                metadata.insert("fanOut".into(), json!(fan_out));
                metadata.insert("hubScore".into(), json!(fan_in));
            }
        }

        BuildOutput {
            graph: Graph {
                nodes: self.nodes,
                edges,
            },
            files: self.covered.into_values().collect(),
            stats: self.stats,
        }
    }
}

/// Builds the graph.
///
/// * `scanned` is every source file the scanner found; those no index covers are kept as
///   uncovered FILE nodes.
/// * `file_info` decides whether a document belongs to the project: it returns the file
///   (hash, size, line count) or `None` for files that are ignored or missing on disk.
///
/// Documents are processed in path order and the first definition of a symbol wins, so
/// the result does not depend on the order the indexes are passed in.
pub fn build_graph(
    inputs: &[IndexInput],
    scanned: &[SourceFile],
    file_info: &mut dyn FnMut(&str) -> Option<SourceFile>,
) -> BuildOutput {
    let mut builder = Builder::default();

    let mut work: Vec<(String, usize, usize)> = Vec::new();
    for (i, input) in inputs.iter().enumerate() {
        for (j, document) in input.index.documents.iter().enumerate() {
            work.push((join_path(&input.root_prefix, &document.relative_path), i, j));
        }
    }
    work.sort();

    for (path, i, j) in work {
        builder.stats.documents += 1;
        if builder.covered.contains_key(&path) {
            builder.stats.documents_duplicate += 1;
            continue;
        }
        let Some(file) = file_info(&path) else {
            builder.stats.documents_ignored += 1;
            continue;
        };
        let index = inputs[i].index;
        builder.add_document(&indexer_label(index), &index.documents[j], file);
    }
    builder.finish(scanned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use protobuf::EnumOrUnknown;
    use scip::types::Relationship;

    const TS: &str = "scip-typescript npm fixture-ts 1.0.0 ";
    const RS: &str = "rust-analyzer cargo codebase-context-graph 0.1.0 ";

    fn occ(range: &[i32], symbol: &str, roles: i32, enclosing: &[i32]) -> Occurrence {
        let mut o = Occurrence::default();
        o.range = range.to_vec();
        o.symbol = symbol.to_string();
        o.symbol_roles = roles;
        o.enclosing_range = enclosing.to_vec();
        o
    }

    fn def(range: &[i32], symbol: &str, enclosing: &[i32]) -> Occurrence {
        occ(range, symbol, 1, enclosing)
    }

    fn reference(range: &[i32], symbol: &str) -> Occurrence {
        occ(range, symbol, 0, &[])
    }

    fn doc(path: &str, occurrences: Vec<Occurrence>) -> Document {
        let mut d = Document::default();
        d.relative_path = path.to_string();
        d.occurrences = occurrences;
        d
    }

    fn with_kind(mut d: Document, symbol: &str, kind: Kind) -> Document {
        let mut info = SymbolInformation::default();
        info.symbol = symbol.to_string();
        info.kind = EnumOrUnknown::new(kind);
        d.symbols.push(info);
        d
    }

    fn index(documents: Vec<Document>) -> Index {
        let mut i = Index::default();
        i.documents = documents;
        let tool = i.metadata.mut_or_insert_default().tool_info.mut_or_insert_default();
        tool.name = "test-indexer".to_string();
        tool.version = "1.0".to_string();
        i
    }

    fn file(path: &str) -> SourceFile {
        SourceFile {
            path: path.to_string(),
            language: "typescript".to_string(),
            size: 10,
            hash: "h".to_string(),
            lines: 20,
        }
    }

    fn build(indexes: &[(&Index, &str)], scanned: &[&str]) -> BuildOutput {
        let inputs: Vec<IndexInput> = indexes
            .iter()
            .map(|&(index, prefix)| IndexInput {
                index,
                root_prefix: prefix.to_string(),
            })
            .collect();
        let scanned: Vec<SourceFile> = scanned.iter().map(|p| file(p)).collect();
        build_graph(&inputs, &scanned, &mut |p| {
            (!p.contains("ignored")).then(|| file(p))
        })
    }

    fn node<'a>(out: &'a BuildOutput, id: &str) -> &'a Node {
        out.graph
            .nodes
            .iter()
            .find(|n| n.id == id)
            .unwrap_or_else(|| panic!("no node {id}"))
    }

    fn edge_of<'a>(out: &'a BuildOutput, source: &str, target: &str, kind: &str) -> Option<&'a Edge> {
        out.graph
            .edges
            .iter()
            .find(|e| e.source_id == source && e.target_id == target && e.edge_type == kind)
    }

    fn count(edge: &Edge) -> i64 {
        edge.metadata["count"].as_i64().unwrap()
    }

    /// Mirrors the real scip-typescript output for the shapes fixture: two classes with a
    /// same-named method, a free function, imports, a call from `draw`, an external package.
    fn shapes_and_app() -> Index {
        let m = |f: &str, rest: &str| format!("{TS}src/`{f}`/{rest}");
        let shapes = doc(
            "src/shapes.ts",
            vec![
                def(&[0, 0, 0], &m("shapes.ts", ""), &[0, 0, 14, 0]),
                def(&[0, 17, 27], &m("shapes.ts", "Renderable#"), &[0, 0, 49]),
                def(&[1, 13, 18], &m("shapes.ts", "Chart#"), &[1, 0, 4, 1]),
                def(&[3, 2, 8], &m("shapes.ts", "Chart#render()."), &[3, 2, 38]),
                def(&[5, 13, 18], &m("shapes.ts", "Table#"), &[5, 0, 7, 1]),
                def(&[6, 2, 8], &m("shapes.ts", "Table#render()."), &[6, 2, 38]),
                def(&[8, 16, 26], &m("shapes.ts", "exportedFn()."), &[8, 0, 63]),
                def(&[8, 27, 28], &m("shapes.ts", "exportedFn().(a)"), &[]),
                reference(&[8, 55, 56], &m("shapes.ts", "exportedFn().(a)")),
                def(&[8, 40, 41], "local 1", &[]),
            ],
        );
        let mut shapes = shapes;
        let mut info = SymbolInformation::default();
        info.symbol = m("shapes.ts", "Chart#");
        let mut rel = Relationship::default();
        rel.symbol = m("shapes.ts", "Renderable#");
        rel.is_implementation = true;
        info.relationships.push(rel);
        shapes.symbols.push(info);

        let app = doc(
            "src/app.ts",
            vec![
                def(&[0, 0, 0], &m("app.ts", ""), &[0, 0, 7, 0]),
                def(&[1, 16, 20], &m("app.ts", "draw()."), &[1, 0, 5, 1]),
                def(&[6, 16, 21], &m("app.ts", "other()."), &[6, 0, 57]),
                reference(&[0, 9, 14], &m("shapes.ts", "Chart#")),
                reference(&[0, 23, 33], &m("shapes.ts", "exportedFn().")),
                reference(&[0, 50, 60], &m("shapes.ts", "")),
                reference(&[2, 16, 21], &m("shapes.ts", "Chart#")),
                reference(&[3, 10, 15], "scip-typescript npm lodash 4.17.0 index/chunk()."),
                reference(&[3, 30, 31], &m("gone.ts", "missing().")),
                reference(&[4, 11, 17], &m("shapes.ts", "Chart#render().")),
                reference(&[4, 24, 30], &m("shapes.ts", "Table#render().")),
                reference(&[4, 35, 45], &m("shapes.ts", "exportedFn().")),
                reference(&[6, 41, 51], &m("shapes.ts", "exportedFn().")),
            ],
        );
        index(vec![shapes, app])
    }

    const CHART: &str = "npm:fixture-ts:src/`shapes.ts`/Chart#";
    const CHART_RENDER: &str = "npm:fixture-ts:src/`shapes.ts`/Chart#render().";
    const TABLE_RENDER: &str = "npm:fixture-ts:src/`shapes.ts`/Table#render().";
    const EXPORTED: &str = "npm:fixture-ts:src/`shapes.ts`/exportedFn().";
    const DRAW: &str = "npm:fixture-ts:src/`app.ts`/draw().";
    const OTHER: &str = "npm:fixture-ts:src/`app.ts`/other().";

    #[test]
    fn definitions_become_nodes_with_the_lines_of_the_whole_definition() {
        let idx = shapes_and_app();
        let out = build(&[(&idx, "")], &["src/shapes.ts", "src/app.ts"]);

        let method = node(&out, CHART_RENDER);
        assert_eq!(method.node_type, "METHOD");
        assert_eq!(method.name, "render");
        assert_eq!(method.file_path.as_deref(), Some("src/shapes.ts"));
        assert_eq!((method.start_line, method.end_line), (Some(4), Some(4)));

        let class = node(&out, CHART);
        assert_eq!((class.start_line, class.end_line), (Some(2), Some(5)));
        assert_eq!(node(&out, EXPORTED).node_type, "FUNCTION");
        assert_eq!(class.metadata["indexer"], "test-indexer 1.0");

        assert_eq!(out.stats.definitions, 8, "modules, parameters and locals are not nodes");
    }

    #[test]
    fn parameters_locals_and_file_modules_do_not_become_nodes() {
        let idx = shapes_and_app();
        let out = build(&[(&idx, "")], &[]);
        assert!(out.graph.nodes.iter().all(|n| !n.id.contains("(a)")));
        assert!(out.graph.nodes.iter().all(|n| !n.id.starts_with("local")));
        let types: BTreeSet<&str> = out.graph.nodes.iter().map(|n| n.node_type.as_str()).collect();
        assert!(!types.contains("NAMESPACE"), "file modules are the FILE nodes");
    }

    #[test]
    fn same_named_methods_are_told_apart_by_resolution_not_by_name() {
        let idx = shapes_and_app();
        let out = build(&[(&idx, "")], &[]);
        assert!(edge_of(&out, DRAW, CHART_RENDER, "CALLS").is_some());
        assert!(edge_of(&out, DRAW, TABLE_RENDER, "CALLS").is_some());
        assert!(edge_of(&out, OTHER, CHART_RENDER, "CALLS").is_none());
        assert!(edge_of(&out, OTHER, TABLE_RENDER, "CALLS").is_none());
    }

    #[test]
    fn references_are_attributed_to_the_innermost_enclosing_definition() {
        let idx = shapes_and_app();
        let out = build(&[(&idx, "")], &[]);
        assert_eq!(count(edge_of(&out, DRAW, EXPORTED, "CALLS").unwrap()), 1);
        assert_eq!(count(edge_of(&out, OTHER, EXPORTED, "CALLS").unwrap()), 1);
        // A type reference inside `draw` is a REFERENCES edge, not a call.
        assert!(edge_of(&out, DRAW, CHART, "REFERENCES").is_some());
        assert!(edge_of(&out, DRAW, CHART, "CALLS").is_none());
    }

    #[test]
    fn module_level_references_belong_to_the_file_and_are_never_calls() {
        let idx = shapes_and_app();
        let out = build(&[(&idx, "")], &[]);
        let import = edge_of(&out, "file:src/app.ts", EXPORTED, "REFERENCES").expect("import edge");
        assert_eq!(import.metadata["lines"], json!([1]));
        assert!(edge_of(&out, "file:src/app.ts", EXPORTED, "CALLS").is_none());
        assert!(edge_of(&out, "file:src/app.ts", CHART, "REFERENCES").is_some());
    }

    #[test]
    fn structure_edges_attach_members_to_their_type_and_the_rest_to_their_file() {
        let idx = shapes_and_app();
        let out = build(&[(&idx, "")], &[]);
        assert!(edge_of(&out, CHART, CHART_RENDER, "CONTAINS").is_some());
        assert!(edge_of(&out, "file:src/shapes.ts", CHART, "DEFINES").is_some());
        assert!(edge_of(&out, "file:src/shapes.ts", CHART_RENDER, "DEFINES").is_none());
        assert!(edge_of(&out, "module:src", "file:src/app.ts", "CONTAINS").is_some());
    }

    #[test]
    fn relationships_become_implements_edges() {
        let idx = shapes_and_app();
        let out = build(&[(&idx, "")], &[]);
        assert!(
            edge_of(&out, CHART, "npm:fixture-ts:src/`shapes.ts`/Renderable#", "IMPLEMENTS").is_some()
        );
    }

    #[test]
    fn external_packages_are_aggregated_and_unresolved_project_references_are_dropped() {
        let idx = shapes_and_app();
        let out = build(&[(&idx, "")], &[]);
        assert_eq!(node(&out, "external:npm:lodash").node_type, "EXTERNAL");
        assert!(edge_of(&out, DRAW, "external:npm:lodash", "USES").is_some());
        assert_eq!(out.stats.references_external, 1);
        assert_eq!(out.stats.references_unresolved, 1);
        assert!(out.graph.nodes.iter().all(|n| !n.id.contains("missing")));
    }

    #[test]
    fn file_and_directory_dependencies_are_derived_from_resolved_edges() {
        let mut idx = shapes_and_app();
        // Move app.ts into another directory to get a directory-level dependency.
        idx.documents[1].relative_path = "cli/app.ts".to_string();
        let out = build(&[(&idx, "")], &[]);
        let files = edge_of(&out, "file:cli/app.ts", "file:src/shapes.ts", "DEPENDS_ON").unwrap();
        // 4 calls (draw: 3, other: 1), 1 type reference from draw, 2 module-level references.
        assert_eq!(count(files), 7);
        assert_eq!(
            count(edge_of(&out, "module:cli", "module:src", "DEPENDS_ON").unwrap()),
            7
        );
        assert!(edge_of(&out, "file:src/shapes.ts", "file:cli/app.ts", "DEPENDS_ON").is_none());
    }

    #[test]
    fn hub_score_is_the_number_of_distinct_dependents() {
        let idx = shapes_and_app();
        let out = build(&[(&idx, "")], &[]);
        let hub = node(&out, EXPORTED);
        // draw, other, and app.ts itself (its import).
        assert_eq!(hub.metadata["fanIn"], 3);
        assert_eq!(hub.metadata["hubScore"], 3);
        assert_eq!(node(&out, TABLE_RENDER).metadata["hubScore"], 1);
        assert_eq!(node(&out, DRAW).metadata["fanOut"], 5);
    }

    #[test]
    fn duplicate_definitions_keep_one_canonical_node_that_references_resolve_to() {
        let main = format!("{RS}main().");
        let a = doc("a.rs", vec![def(&[0, 3, 7], &main, &[0, 0, 2, 1])]);
        let b = doc("b.rs", vec![def(&[4, 3, 7], &main, &[4, 0, 6, 1])]);
        let c = doc(
            "c.rs",
            vec![
                def(&[0, 3, 4], &format!("{RS}run()."), &[0, 0, 3, 1]),
                reference(&[1, 4, 8], &main),
            ],
        );
        let idx = index(vec![b, c, a]);
        let out = build(&[(&idx, "")], &[]);

        let canonical = "cargo:codebase-context-graph:main().";
        assert_eq!(node(&out, canonical).file_path.as_deref(), Some("a.rs"), "first in path order wins");
        let second = node(&out, "cargo:codebase-context-graph:main().~2");
        assert_eq!(second.file_path.as_deref(), Some("b.rs"));
        assert_eq!(second.metadata["duplicateOf"], canonical);
        assert!(edge_of(&out, "cargo:codebase-context-graph:run().", canonical, "CALLS").is_some());
        assert_eq!(out.stats.duplicate_definitions, 1);
    }

    #[test]
    fn rust_methods_are_contained_by_their_struct() {
        let config = format!("{RS}config/Config#");
        let new = format!("{RS}config/impl#[Config]new().");
        let d = doc(
            "config.rs",
            vec![
                def(&[0, 0, 110, 0], &format!("{RS}config/"), &[0, 0, 110, 0]),
                def(&[62, 11, 17], &config, &[61, 0, 69, 1]),
                def(&[75, 7, 10], &new, &[75, 0, 80, 1]),
            ],
        );
        let d = with_kind(d, &config, Kind::Struct);
        let d = with_kind(d, &new, Kind::StaticMethod);
        let out = build(&[(&index(vec![d]), "")], &[]);
        assert_eq!(node(&out, "cargo:codebase-context-graph:config/Config#").node_type, "STRUCT");
        assert!(
            edge_of(
                &out,
                "cargo:codebase-context-graph:config/Config#",
                "cargo:codebase-context-graph:config/impl#[Config]new().",
                "CONTAINS"
            )
            .is_some()
        );
    }

    #[test]
    fn indexer_roots_below_the_project_root_are_rebased() {
        let idx = index(vec![doc(
            "main.rs",
            vec![def(&[0, 3, 7], &format!("{RS}main()."), &[0, 0, 2, 1])],
        )]);
        let out = build(&[(&idx, "rust-cli")], &[]);
        let main = node(&out, "cargo:codebase-context-graph:main().");
        assert_eq!(main.file_path.as_deref(), Some("rust-cli/main.rs"));
        assert!(edge_of(&out, "module:rust-cli", "file:rust-cli/main.rs", "CONTAINS").is_some());
    }

    #[test]
    fn ignored_or_missing_documents_are_dropped_and_counted() {
        let idx = index(vec![
            doc("ignored/a.ts", vec![def(&[0, 0, 1], &format!("{TS}src/`a.ts`/f()."), &[0, 0, 1, 0])]),
            doc("src/b.ts", vec![]),
        ]);
        let out = build(&[(&idx, "")], &[]);
        assert_eq!(out.stats.documents, 2);
        assert_eq!(out.stats.documents_ignored, 1);
        assert!(out.graph.nodes.iter().all(|n| n.file_path.as_deref() != Some("ignored/a.ts")));
    }

    #[test]
    fn the_same_file_indexed_twice_is_only_ingested_once() {
        let make = || {
            index(vec![doc(
                "src/b.ts",
                vec![def(&[0, 0, 1], &format!("{TS}src/`b.ts`/f()."), &[0, 0, 1, 0])],
            )])
        };
        let (first, second) = (make(), make());
        let out = build(&[(&first, ""), (&second, "")], &[]);
        assert_eq!(out.stats.documents_duplicate, 1);
        assert_eq!(out.stats.duplicate_definitions, 0);
    }

    #[test]
    fn files_no_indexer_covered_stay_in_the_graph_flagged_as_uncovered() {
        let idx = index(vec![doc("src/a.ts", vec![])]);
        let out = build(&[(&idx, "")], &["src/a.ts", "src/never_indexed.ts"]);

        assert_eq!(node(&out, "file:src/a.ts").metadata["covered"], true);
        let gap = node(&out, "file:src/never_indexed.ts");
        assert_eq!(gap.metadata["covered"], false);

        let by_path: HashMap<&str, &FileRecord> =
            out.files.iter().map(|f| (f.file.path.as_str(), f)).collect();
        assert_eq!(by_path["src/a.ts"].indexed_by.as_deref(), Some("test-indexer 1.0"));
        assert_eq!(by_path["src/never_indexed.ts"].indexed_by, None);
    }

    #[test]
    fn building_twice_gives_identical_output() {
        let idx = shapes_and_app();
        let render = |out: &BuildOutput| {
            serde_json::to_string(&json!({ "n": out.graph.nodes, "e": out.graph.edges })).unwrap()
        };
        let first = render(&build(&[(&idx, "")], &["src/shapes.ts"]));
        let second = render(&build(&[(&idx, "")], &["src/shapes.ts"]));
        assert_eq!(first, second);
    }

    #[test]
    fn line_numbers_follow_scip_conventions() {
        let whole_file = span_from(&[0, 0, 14, 0]).unwrap();
        assert_eq!((start_line(&whole_file), end_line(&whole_file)), (1, 14), "ends before column 0");
        let block = span_from(&[1, 0, 5, 1]).unwrap();
        assert_eq!((start_line(&block), end_line(&block)), (2, 6));
        let single = span_from(&[3, 2, 38]).unwrap();
        assert_eq!((start_line(&single), end_line(&single)), (4, 4));
        assert!(span_from(&[1, 2]).is_none());
    }
}
