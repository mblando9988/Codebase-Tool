//! The MCP tools. Each one declares what it accepts and what it returns, and runs its queries;
//! everything about size, paging, validation and freshness is done by `mcp.rs`.

use crate::config;
use crate::indexers;
use crate::mcp::{
    Args, Column, Ctx, Doc, Field, ID_MAX, PATH_MAX, Param, Table, TableSpec, ToolDef, ToolError, col, echo, file_hash,
};
use crate::scanner;
use crate::symbols::SYMBOL_TYPES;
use rusqlite::types::{Value as SqlValue, ValueRef};
use rusqlite::{Connection, params_from_iter};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::path::Path;

/// Nodes a single `trace` may visit before it stops and says so.
pub const MAX_VISITED: usize = 5000;
/// Lines a single `read_source` call may ask for.
pub const MAX_READ_LINES: i64 = 400;
/// Call-site lines listed per relation.
const MAX_SITES: usize = 5;
const MAX_LINE_NUMBER: i64 = 10_000_000;

pub fn registry() -> Vec<ToolDef> {
    vec![overview_def(), find_symbols_def(), file_outline_def(), symbol_detail_def(), trace_def(), read_source_def()]
}

// ------------------------------------------------------------------------------------------
// Helpers.
// ------------------------------------------------------------------------------------------

fn db_error(e: rusqlite::Error) -> ToolError {
    ToolError::Internal(format!("database error: {e}"))
}

fn cell(value: ValueRef) -> Value {
    match value {
        ValueRef::Null | ValueRef::Blob(_) => Value::Null,
        ValueRef::Integer(n) => json!(n),
        ValueRef::Real(f) => json!(f),
        ValueRef::Text(t) => json!(String::from_utf8_lossy(t)),
    }
}

fn query(db: &Connection, sql: &str, args: Vec<SqlValue>) -> Result<Vec<Vec<Value>>, ToolError> {
    let mut statement = db.prepare_cached(sql).map_err(db_error)?;
    let width = statement.column_count();
    let mut rows = statement.query(params_from_iter(args)).map_err(db_error)?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().map_err(db_error)? {
        let mut values = Vec::with_capacity(width);
        for index in 0..width {
            values.push(cell(row.get_ref(index).map_err(db_error)?));
        }
        out.push(values);
    }
    Ok(out)
}

fn count(db: &Connection, sql: &str, args: Vec<SqlValue>) -> Result<i64, ToolError> {
    let rows = query(db, sql, args)?;
    rows.first().and_then(|r| r.first()).and_then(Value::as_i64).ok_or_else(|| {
        ToolError::Internal("a count query returned no number".into())
    })
}

fn text(value: &str) -> SqlValue {
    SqlValue::Text(value.to_string())
}

fn int(value: i64) -> SqlValue {
    SqlValue::Integer(value)
}

/// Makes `%`, `_` and `\` match themselves in a LIKE pattern that uses `ESCAPE '\'`.
pub fn escape_like(value: &str) -> String {
    value.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
}

fn in_list(types: &[&str]) -> String {
    types.iter().map(|t| format!("'{t}'")).collect::<Vec<_>>().join(",")
}

fn meta(db: &Connection, key: &str) -> Option<String> {
    query(db, "SELECT value FROM meta WHERE key = ?", vec![text(key)])
        .ok()?
        .first()?
        .first()?
        .as_str()
        .map(String::from)
}

/// A path relative to the project root: no drive letter or leading `/`, no `..`. A leading
/// `./` and backslashes are tolerated.
pub fn clean_relative(raw: &str) -> Result<String, ToolError> {
    let mut path = raw.replace('\\', "/");
    while let Some(rest) = path.strip_prefix("./") {
        path = rest.to_string();
    }
    let drive = path.as_bytes().get(1) == Some(&b':') && path.as_bytes()[0].is_ascii_alphabetic();
    if path.starts_with('/') || drive {
        return Err(ToolError::Invalid("use a path relative to the project root".into()));
    }
    if path.split('/').any(|segment| segment == "..") {
        return Err(ToolError::Invalid("`..` is not allowed in a path".into()));
    }
    if path.is_empty() {
        return Err(ToolError::Invalid("the path is empty".into()));
    }
    Ok(path)
}

/// `clean_relative`, plus repeated and trailing slashes removed: the form stored in the index.
pub fn normalize_path(raw: &str) -> Result<String, ToolError> {
    let cleaned = clean_relative(raw)?;
    let parts: Vec<&str> = cleaned.split('/').filter(|s| !s.is_empty() && *s != ".").collect();
    if parts.is_empty() {
        return Err(ToolError::Invalid("the path is empty".into()));
    }
    Ok(parts.join("/"))
}

fn not_found_file(db: &Connection, path: &str) -> ToolError {
    let base = path.rsplit('/').next().unwrap_or(path);
    let hints = query(
        db,
        "SELECT file_path FROM file_manifest WHERE file_path LIKE ? ESCAPE '\\' ORDER BY file_path LIMIT 5",
        vec![text(&format!("%{}%", escape_like(base)))],
    )
    .unwrap_or_default()
    .into_iter()
    .filter_map(|r| r.first().and_then(Value::as_str).map(String::from))
    .collect();
    ToolError::NotFound { what: format!("`{}` is not an indexed file", echo(path)), hints }
}

fn not_found_node(db: &Connection, id: &str) -> ToolError {
    let tail = id
        .rsplit(|c: char| !(c.is_alphanumeric() || c == '_'))
        .find(|s| !s.is_empty())
        .unwrap_or("");
    let hints = if tail.is_empty() {
        Vec::new()
    } else {
        query(db, "SELECT id FROM nodes WHERE name = ? ORDER BY id LIMIT 5", vec![text(tail)])
            .unwrap_or_default()
            .into_iter()
            .filter_map(|r| r.first().and_then(Value::as_str).map(String::from))
            .collect()
    };
    ToolError::NotFound { what: "no node has that id".into(), hints }
}

/// The first few lines of a reference, from the JSON list stored on an edge.
fn sites(cell: &Value) -> Value {
    let lines: Vec<i64> = cell
        .as_str()
        .and_then(|s| serde_json::from_str::<Vec<i64>>(s).ok())
        .unwrap_or_default();
    json!(lines.into_iter().take(MAX_SITES).collect::<Vec<_>>())
}

fn read_config(root: &Path) -> config::Config {
    std::fs::read_to_string(config::config_path(root))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| config::default_config(root))
}

// ------------------------------------------------------------------------------------------
// overview
// ------------------------------------------------------------------------------------------

const OVERVIEW_HEAD: &[Field] = &[
    Field { name: "project", ty: "string" },
    Field { name: "schema_version", ty: "integer" },
    Field { name: "files", ty: "integer" },
    Field { name: "covered", ty: "integer" },
    Field { name: "nodes", ty: "integer" },
    Field { name: "edges", ty: "integer" },
    Field { name: "fresh", ty: "boolean" },
    Field { name: "long_ids", ty: "integer" },
];
const OVERVIEW_TABLES: &[TableSpec] = &[
    TableSpec { name: "languages", columns: &[col("language", "string"), col("files", "integer"), col("covered", "integer")] },
    TableSpec { name: "kinds", columns: &[col("kind", "string"), col("count", "integer")] },
    TableSpec { name: "edge_types", columns: &[col("type", "string"), col("count", "integer")] },
    TableSpec {
        name: "indexers",
        columns: &[
            col("indexer", "string"),
            col("root", "string"),
            col("status", "string"),
            col("documents", "integer"),
            col("message", "string"),
        ],
    },
    TableSpec { name: "uncovered", columns: &[col("file", "string"), col("language", "string")] },
    TableSpec { name: "changes", columns: &[col("file", "string"), col("state", "string")] },
];

fn overview_def() -> ToolDef {
    ToolDef {
        name: "overview",
        title: "Project overview",
        description: "Start here. Coverage by language, node and edge counts, the indexers that ran, \
files no indexer covered, and whether any file changed since indexing (reads every indexed file to check).",
        params: vec![],
        paginated: false,
        head: OVERVIEW_HEAD,
        tables: OVERVIEW_TABLES,
        run: overview,
    }
}

/// Files whose content differs from what was indexed, files that vanished, and files that
/// would be indexed now but were not then.
fn drift(ctx: &Ctx, notes: &mut Vec<String>) -> Result<Vec<Vec<Value>>, ToolError> {
    let manifest = query(ctx.db, "SELECT file_path, content_hash FROM file_manifest ORDER BY file_path", vec![])?;
    let mut changes: Vec<(String, &'static str)> = Vec::new();
    let mut known: HashSet<String> = HashSet::new();
    for row in &manifest {
        let (Some(path), Some(hash)) = (row[0].as_str(), row[1].as_str()) else { continue };
        known.insert(path.to_string());
        match std::fs::read(ctx.root.join(path)) {
            Ok(bytes) if file_hash(&bytes) == hash => {}
            Ok(_) => changes.push((path.to_string(), "changed")),
            Err(_) => changes.push((path.to_string(), "missing")),
        }
    }
    let config = read_config(ctx.root);
    let specs = indexers::effective_specs(&config);
    match scanner::scan_project(
        ctx.root,
        &config,
        &|path| indexers::language_of(path, &specs),
        &indexers::marker_names(&specs),
    ) {
        Ok(scan) => {
            for file in scan.files {
                if !known.contains(&file.path) {
                    changes.push((file.path, "new"));
                }
            }
        }
        Err(why) => notes.push(format!("could not scan for new files: {why}")),
    }
    changes.sort();
    Ok(changes.into_iter().map(|(path, state)| vec![json!(path), json!(state)]).collect())
}

fn overview(ctx: &Ctx, _args: &Args) -> Result<Doc, ToolError> {
    let db = ctx.db;
    let mut notes = Vec::new();
    let window = ctx.window;

    let files = count(db, "SELECT COUNT(*) FROM file_manifest", vec![])?;
    let covered = count(db, "SELECT COUNT(*) FROM file_manifest WHERE indexed_by IS NOT NULL", vec![])?;
    let nodes = count(db, "SELECT COUNT(*) FROM nodes", vec![])?;
    let edges = count(db, "SELECT COUNT(*) FROM edges", vec![])?;
    let long_ids = count(db, &format!("SELECT COUNT(*) FROM nodes WHERE LENGTH(id) > {ID_MAX}"), vec![])?;
    if long_ids > 0 {
        notes.push(format!("{long_ids} node id(s) are longer than {ID_MAX} characters and cannot be passed to other tools"));
    }
    if covered < files {
        notes.push(format!("{} of {files} files have no semantic data: see `uncovered`", files - covered));
    }

    let languages = query(
        db,
        "SELECT language, COUNT(*), SUM(CASE WHEN indexed_by IS NULL THEN 0 ELSE 1 END)
           FROM file_manifest GROUP BY language ORDER BY language LIMIT ?",
        vec![int(window)],
    )?;
    let kinds = query(db, "SELECT type, COUNT(*) FROM nodes GROUP BY type ORDER BY COUNT(*) DESC, type LIMIT ?", vec![int(window)])?;
    let edge_types = query(db, "SELECT type, COUNT(*) FROM edges GROUP BY type ORDER BY COUNT(*) DESC, type LIMIT ?", vec![int(window)])?;
    let runs = query(
        db,
        "SELECT indexer, root, status, documents, message FROM index_runs ORDER BY id LIMIT ?",
        vec![int(window)],
    )?;
    let runs_total = count(db, "SELECT COUNT(*) FROM index_runs", vec![])?;
    let uncovered = query(
        db,
        "SELECT file_path, language FROM file_manifest WHERE indexed_by IS NULL ORDER BY file_path LIMIT ?",
        vec![int(window)],
    )?;
    let changes = drift(ctx, &mut notes)?;
    let changes_total = changes.len() as i64;

    Ok(Doc {
        head: vec![
            ("project", json!(meta(db, "project_name").unwrap_or_default())),
            ("schema_version", json!(meta(db, "schema_version").and_then(|v| v.parse::<i64>().ok()).unwrap_or(0))),
            ("files", json!(files)),
            ("covered", json!(covered)),
            ("nodes", json!(nodes)),
            ("edges", json!(edges)),
            ("fresh", json!(changes_total == 0)),
            ("long_ids", json!(long_ids)),
        ],
        tables: vec![
            Table { name: "languages", columns: OVERVIEW_TABLES[0].columns, total: languages.len() as i64, rows: languages },
            Table { name: "kinds", columns: OVERVIEW_TABLES[1].columns, total: kinds.len() as i64, rows: kinds },
            Table { name: "edge_types", columns: OVERVIEW_TABLES[2].columns, total: edge_types.len() as i64, rows: edge_types },
            Table { name: "indexers", columns: OVERVIEW_TABLES[3].columns, total: runs_total, rows: runs },
            Table {
                name: "uncovered",
                columns: OVERVIEW_TABLES[4].columns,
                total: files - covered,
                rows: uncovered,
            },
            Table {
                name: "changes",
                columns: OVERVIEW_TABLES[5].columns,
                total: changes_total,
                rows: changes.into_iter().take(window as usize).collect(),
            },
        ],
        notes,
    })
}

// ------------------------------------------------------------------------------------------
// find_symbols
// ------------------------------------------------------------------------------------------

const SYMBOL_COLUMNS: &[Column] = &[
    col("id", "string"),
    col("kind", "string"),
    col("name", "string"),
    col("file", "string|null"),
    col("start", "integer|null"),
    col("end", "integer|null"),
    col("fan_in", "integer"),
];
const SYMBOL_TABLE: &[TableSpec] = &[TableSpec { name: "symbols", columns: SYMBOL_COLUMNS }];

fn find_symbols_def() -> ToolDef {
    ToolDef {
        name: "find_symbols",
        title: "Find symbols by name",
        description: "Find definitions by name: a case-insensitive (ASCII) substring of the name or the \
qualified name. Ranked exact name, then name prefix, then most depended-on. The ids it returns are the \
handles for symbol_detail, trace and read_source.",
        params: vec![
            Param::text("query", 1, 200, "Text to look for in symbol names; % and _ match themselves."),
            Param::choice("kind", SYMBOL_TYPES, "Only this kind of symbol.").optional(),
            Param::text("path_prefix", 1, PATH_MAX, "Only symbols in files whose path starts with this (case-sensitive).").optional(),
        ],
        paginated: true,
        head: &[],
        tables: SYMBOL_TABLE,
        run: find_symbols,
    }
}

fn find_symbols(ctx: &Ctx, args: &Args) -> Result<Doc, ToolError> {
    let query_text = args.text("query");
    let like = format!("%{}%", escape_like(query_text));
    let mut condition = format!(
        "n.type IN ({}) AND (n.name LIKE ? ESCAPE '\\' \
         OR COALESCE(json_extract(n.metadata, '$.qualified'), '') LIKE ? ESCAPE '\\')",
        in_list(SYMBOL_TYPES)
    );
    let mut bind = vec![text(&like), text(&like)];
    if let Some(kind) = args.opt_choice("kind") {
        condition.push_str(" AND n.type = ?");
        bind.push(text(kind));
    }
    if let Some(prefix) = args.opt_text("path_prefix") {
        // Exact, case-sensitive comparison: LIKE would ignore case in file names.
        let prefix = clean_relative(prefix)?;
        condition.push_str(" AND substr(n.file_path, 1, ?) = ?");
        bind.push(int(prefix.chars().count() as i64));
        bind.push(text(&prefix));
    }
    let total = count(ctx.db, &format!("SELECT COUNT(*) FROM nodes n WHERE {condition}"), bind.clone())?;

    let mut rows_bind = bind;
    rows_bind.push(text(query_text));
    rows_bind.push(text(&format!("{}%", escape_like(query_text))));
    rows_bind.push(int(ctx.window));
    rows_bind.push(int(ctx.offset));
    let rows = query(
        ctx.db,
        &format!(
            "SELECT n.id, n.type, n.name, n.file_path, n.start_line, n.end_line,
                    COALESCE(json_extract(n.metadata, '$.fanIn'), 0)
               FROM nodes n WHERE {condition}
              ORDER BY (lower(n.name) = lower(?)) DESC,
                       (n.name LIKE ? ESCAPE '\\') DESC,
                       COALESCE(json_extract(n.metadata, '$.fanIn'), 0) DESC, n.name, n.id
              LIMIT ? OFFSET ?"
        ),
        rows_bind,
    )?;
    let mut notes = Vec::new();
    if total == 0 {
        notes.push("no symbol matched: the query is a case-insensitive substring of a name, not a pattern".to_string());
    }
    Ok(Doc { head: vec![], tables: vec![Table { name: "symbols", columns: SYMBOL_COLUMNS, rows, total }], notes })
}

// ------------------------------------------------------------------------------------------
// file_outline
// ------------------------------------------------------------------------------------------

const OUTLINE_COLUMNS: &[Column] = &[
    col("kind", "string"),
    col("name", "string"),
    col("start", "integer|null"),
    col("end", "integer|null"),
    col("id", "string"),
    col("signature", "string|null"),
];
const OUTLINE_TABLE: &[TableSpec] = &[TableSpec { name: "symbols", columns: OUTLINE_COLUMNS }];

fn file_outline_def() -> ToolDef {
    ToolDef {
        name: "file_outline",
        title: "Outline of one file",
        description: "The definitions in one indexed file in source order, with line ranges and \
signatures. A file no indexer covered says so (covered=false): that is not the same as a file with \
no definitions.",
        params: vec![Param::text("path", 1, PATH_MAX, "File path relative to the project root.")],
        paginated: true,
        head: &[
            Field { name: "path", ty: "string" },
            Field { name: "language", ty: "string" },
            Field { name: "lines", ty: "integer" },
            Field { name: "covered", ty: "boolean" },
            Field { name: "indexed_by", ty: "string|null" },
        ],
        tables: OUTLINE_TABLE,
        run: file_outline,
    }
}

fn file_outline(ctx: &Ctx, args: &Args) -> Result<Doc, ToolError> {
    let path = normalize_path(args.text("path"))?;
    let file = query(
        ctx.db,
        "SELECT language, lines, indexed_by FROM file_manifest WHERE file_path = ?",
        vec![text(&path)],
    )?;
    let Some(file) = file.first() else {
        return Err(not_found_file(ctx.db, &path));
    };
    let covered = !file[2].is_null();
    let total = count(
        ctx.db,
        "SELECT COUNT(*) FROM nodes WHERE file_path = ? AND type <> 'FILE'",
        vec![text(&path)],
    )?;
    let rows = query(
        ctx.db,
        "SELECT type, name, start_line, end_line, id, json_extract(metadata, '$.signature')
           FROM nodes WHERE file_path = ? AND type <> 'FILE'
          ORDER BY start_line, end_line DESC, id LIMIT ? OFFSET ?",
        vec![text(&path), int(ctx.window), int(ctx.offset)],
    )?;
    let mut notes = Vec::new();
    if !covered {
        notes.push("no indexer covered this file: it has no symbol data, which is not the same as having no definitions".to_string());
    }
    Ok(Doc {
        head: vec![
            ("path", json!(path)),
            ("language", file[0].clone()),
            ("lines", file[1].clone()),
            ("covered", json!(covered)),
            ("indexed_by", file[2].clone()),
        ],
        tables: vec![Table { name: "symbols", columns: OUTLINE_COLUMNS, rows, total }],
        notes,
    })
}

// ------------------------------------------------------------------------------------------
// symbol_detail
// ------------------------------------------------------------------------------------------

const MEMBER_COLUMNS: &[Column] = &[
    col("kind", "string"),
    col("name", "string"),
    col("start", "integer|null"),
    col("end", "integer|null"),
    col("id", "string"),
];
const RELATION_COLUMNS: &[Column] = &[
    col("rel", "string"),
    col("id", "string"),
    col("name", "string"),
    col("kind", "string"),
    col("file", "string|null"),
    col("line", "integer|null"),
    col("count", "integer"),
    col("sites", "array"),
];
const DETAIL_TABLES: &[TableSpec] = &[
    TableSpec { name: "members", columns: MEMBER_COLUMNS },
    TableSpec { name: "dependents", columns: RELATION_COLUMNS },
    TableSpec { name: "dependencies", columns: RELATION_COLUMNS },
];

fn symbol_detail_def() -> ToolDef {
    ToolDef {
        name: "symbol_detail",
        title: "One symbol in detail",
        description: "One node by id: signature, doc, location and container, its members, what depends \
on it and what it depends on. In the relation tables file and line are where the other node is defined \
and sites are the lines of the reference. duplicate means another definition of the same symbol came \
first; its id ends in ~N. Tables are capped; use trace for the full closure.",
        params: vec![Param::text("id", 1, ID_MAX, "Node id, from find_symbols, file_outline or trace.")],
        paginated: false,
        head: &[
            Field { name: "id", ty: "string" },
            Field { name: "kind", ty: "string" },
            Field { name: "name", ty: "string" },
            Field { name: "qualified", ty: "string|null" },
            Field { name: "file", ty: "string|null" },
            Field { name: "start", ty: "integer|null" },
            Field { name: "end", ty: "integer|null" },
            Field { name: "language", ty: "string|null" },
            Field { name: "signature", ty: "string|null" },
            Field { name: "doc", ty: "string|null" },
            Field { name: "container", ty: "string|null" },
            Field { name: "duplicate", ty: "boolean" },
            Field { name: "fan_in", ty: "integer" },
            Field { name: "fan_out", ty: "integer" },
        ],
        tables: DETAIL_TABLES,
        run: symbol_detail,
    }
}

struct NodeRow {
    id: String,
    kind: String,
    name: String,
    file: Option<String>,
    start: Option<i64>,
    end: Option<i64>,
    language: Option<String>,
    metadata: Value,
}

fn load_node(db: &Connection, id: &str) -> Result<Option<NodeRow>, ToolError> {
    let rows = query(
        db,
        "SELECT id, type, name, file_path, start_line, end_line, language, metadata FROM nodes WHERE id = ?",
        vec![text(id)],
    )?;
    Ok(rows.into_iter().next().map(|r| NodeRow {
        id: r[0].as_str().unwrap_or_default().to_string(),
        kind: r[1].as_str().unwrap_or_default().to_string(),
        name: r[2].as_str().unwrap_or_default().to_string(),
        file: r[3].as_str().map(String::from),
        start: r[4].as_i64(),
        end: r[5].as_i64(),
        language: r[6].as_str().map(String::from),
        metadata: r[7].as_str().and_then(|m| serde_json::from_str(m).ok()).unwrap_or(Value::Null),
    }))
}

const RELATION_ORDER: &str = "CASE e.type WHEN 'CALLS' THEN 0 WHEN 'IMPLEMENTS' THEN 1 \
WHEN 'REFERENCES' THEN 2 WHEN 'USES' THEN 3 ELSE 4 END";
const RELATION_TYPES: &str = "'CALLS','IMPLEMENTS','REFERENCES','USES','DEPENDS_ON'";

fn relations(ctx: &Ctx, id: &str, incoming: bool) -> Result<Table, ToolError> {
    let (own, other) = if incoming { ("target_id", "source_id") } else { ("source_id", "target_id") };
    let total = count(
        ctx.db,
        &format!("SELECT COUNT(*) FROM edges e WHERE e.{own} = ? AND e.type IN ({RELATION_TYPES})"),
        vec![text(id)],
    )?;
    let rows = query(
        ctx.db,
        &format!(
            "SELECT e.type, o.id, o.name, o.type, o.file_path, o.start_line,
                    COALESCE(json_extract(e.metadata, '$.count'), 1) AS c,
                    json_extract(e.metadata, '$.lines')
               FROM edges e JOIN nodes o ON o.id = e.{other}
              WHERE e.{own} = ? AND e.type IN ({RELATION_TYPES})
              ORDER BY {RELATION_ORDER}, c DESC, o.name, o.id LIMIT ?"
        ),
        vec![text(id), int(ctx.window)],
    )?
    .into_iter()
    .map(|mut r| {
        r[7] = sites(&r[7]);
        r
    })
    .collect();
    Ok(Table {
        name: if incoming { "dependents" } else { "dependencies" },
        columns: RELATION_COLUMNS,
        rows,
        total,
    })
}

fn symbol_detail(ctx: &Ctx, args: &Args) -> Result<Doc, ToolError> {
    let id = args.text("id");
    let Some(node) = load_node(ctx.db, id)? else {
        return Err(not_found_node(ctx.db, id));
    };
    let meta_str = |key: &str| node.metadata.get(key).and_then(Value::as_str).map(String::from);
    let meta_int = |key: &str| node.metadata.get(key).and_then(Value::as_i64).unwrap_or(0);
    let container = query(
        ctx.db,
        "SELECT source_id FROM edges WHERE target_id = ? AND type IN ('CONTAINS','DEFINES') ORDER BY source_id LIMIT 1",
        vec![text(&node.id)],
    )?
    .first()
    .and_then(|r| r[0].as_str().map(String::from));

    let members_total = count(
        ctx.db,
        "SELECT COUNT(*) FROM edges WHERE source_id = ? AND type IN ('CONTAINS','DEFINES')",
        vec![text(&node.id)],
    )?;
    let members = query(
        ctx.db,
        "SELECT n.type, n.name, n.start_line, n.end_line, n.id
           FROM edges e JOIN nodes n ON n.id = e.target_id
          WHERE e.source_id = ? AND e.type IN ('CONTAINS','DEFINES')
          ORDER BY n.start_line, n.end_line DESC, n.id LIMIT ?",
        vec![text(&node.id), int(ctx.window)],
    )?;

    let mut notes = Vec::new();
    if node.kind == "FILE" && node.metadata.get("covered") == Some(&json!(false)) {
        notes.push("no indexer covered this file: it has no symbol data".to_string());
    }
    Ok(Doc {
        head: vec![
            ("id", json!(node.id)),
            ("kind", json!(node.kind)),
            ("name", json!(node.name)),
            ("qualified", json!(meta_str("qualified"))),
            ("file", json!(node.file)),
            ("start", json!(node.start)),
            ("end", json!(node.end)),
            ("language", json!(node.language)),
            ("signature", json!(meta_str("signature"))),
            ("doc", json!(meta_str("doc"))),
            ("container", json!(container)),
            ("duplicate", json!(node.metadata.get("duplicateOf").is_some())),
            ("fan_in", json!(meta_int("fanIn"))),
            ("fan_out", json!(meta_int("fanOut"))),
        ],
        tables: vec![
            Table { name: "members", columns: MEMBER_COLUMNS, rows: members, total: members_total },
            relations(ctx, &node.id, true)?,
            relations(ctx, &node.id, false)?,
        ],
        notes,
    })
}

// ------------------------------------------------------------------------------------------
// trace
// ------------------------------------------------------------------------------------------

const TRACE_COLUMNS: &[Column] = &[
    col("n", "integer"),
    col("from", "integer"),
    col("depth", "integer"),
    col("rel", "string"),
    col("id", "string"),
    col("kind", "string"),
    col("name", "string"),
    col("file", "string|null"),
    col("line", "integer|null"),
    col("count", "integer"),
    col("sites", "array"),
];
const TRACE_TABLE: &[TableSpec] = &[TableSpec { name: "nodes", columns: TRACE_COLUMNS }];

fn trace_def() -> ToolDef {
    ToolDef {
        name: "trace",
        title: "Follow dependents or dependencies",
        description: "Breadth-first closure from one node: dependents (who uses it) or dependencies (what \
it uses), up to depth 4. Each node appears once, at its shortest depth; from is the n of the row that \
reached it (0 is the start). file and line are where the row's node is defined; sites are the lines of \
the reference, in the file of whichever of the two nodes makes it. edges=calls follows calls only; \
all adds references, implements and external use.",
        params: vec![
            Param::text("id", 1, ID_MAX, "Node id to start from."),
            Param::choice("direction", &["dependents", "dependencies"], "dependents: who uses it. dependencies: what it uses."),
            Param::int("depth", 1, 4, "How many steps to follow.").default(1),
            Param::choice("edges", &["calls", "all"], "calls: call edges only. all: also references, implements, external use.")
                .default("all"),
        ],
        paginated: true,
        head: &[
            Field { name: "id", ty: "string" },
            Field { name: "kind", ty: "string" },
            Field { name: "name", ty: "string" },
            Field { name: "file", ty: "string|null" },
            Field { name: "direction", ty: "string" },
            Field { name: "edges", ty: "string" },
            Field { name: "depth", ty: "integer" },
            Field { name: "levels", ty: "array" },
            Field { name: "complete", ty: "boolean" },
        ],
        tables: TRACE_TABLE,
        run: trace,
    }
}

struct Hit {
    from: i64,
    depth: i64,
    rel: String,
    id: String,
    kind: String,
    name: String,
    file: Option<String>,
    line: Option<i64>,
    count: i64,
    sites: Value,
}

/// Breadth-first closure. Returns the nodes in discovery order and whether the walk was complete.
fn closure(
    db: &Connection,
    root: &str,
    dependents: bool,
    edge_types: &[&str],
    max_depth: i64,
    cap: usize,
) -> Result<(Vec<Hit>, bool), ToolError> {
    let (own, other) = if dependents { ("target_id", "source_id") } else { ("source_id", "target_id") };
    let sql = format!(
        "SELECT o.id, o.type, o.name, o.file_path, o.start_line, e.type,
                COALESCE(json_extract(e.metadata, '$.count'), 1) AS c,
                json_extract(e.metadata, '$.lines')
           FROM edges e JOIN nodes o ON o.id = e.{other}
          WHERE e.{own} = ? AND e.type IN ({})
          ORDER BY {RELATION_ORDER}, c DESC, o.id",
        in_list(edge_types)
    );
    let mut seen: HashSet<String> = HashSet::from([root.to_string()]);
    let mut hits: Vec<Hit> = Vec::new();
    let mut frontier: Vec<(String, i64)> = vec![(root.to_string(), 0)];
    for depth in 1..=max_depth {
        let mut next = Vec::new();
        for (parent, parent_n) in &frontier {
            for row in query(db, &sql, vec![text(parent)])? {
                let id = row[0].as_str().unwrap_or_default().to_string();
                if seen.contains(&id) {
                    continue;
                }
                if hits.len() >= cap {
                    return Ok((hits, false));
                }
                seen.insert(id.clone());
                hits.push(Hit {
                    from: *parent_n,
                    depth,
                    rel: row[5].as_str().unwrap_or_default().to_string(),
                    id: id.clone(),
                    kind: row[1].as_str().unwrap_or_default().to_string(),
                    name: row[2].as_str().unwrap_or_default().to_string(),
                    file: row[3].as_str().map(String::from),
                    line: row[4].as_i64(),
                    count: row[6].as_i64().unwrap_or(1),
                    sites: sites(&row[7]),
                });
                next.push((id, hits.len() as i64));
            }
        }
        if next.is_empty() {
            break;
        }
        frontier = next;
    }
    Ok((hits, true))
}

fn trace(ctx: &Ctx, args: &Args) -> Result<Doc, ToolError> {
    let id = args.text("id");
    let Some(root) = load_node(ctx.db, id)? else {
        return Err(not_found_node(ctx.db, id));
    };
    let direction = args.choice("direction");
    let edges = args.choice("edges");
    let depth = args.int("depth");
    let edge_types: &[&str] = match edges {
        "calls" => &["CALLS"],
        _ => &["CALLS", "REFERENCES", "IMPLEMENTS", "USES"],
    };
    let (hits, complete) = closure(ctx.db, &root.id, direction == "dependents", edge_types, depth, MAX_VISITED)?;

    let mut levels = vec![0i64; hits.iter().map(|h| h.depth).max().unwrap_or(0) as usize];
    for hit in &hits {
        levels[hit.depth as usize - 1] += 1;
    }
    let total = hits.len() as i64;
    let rows: Vec<Vec<Value>> = hits
        .iter()
        .enumerate()
        .skip(ctx.offset as usize)
        .take(ctx.window as usize)
        .map(|(index, h)| {
            vec![
                json!(index as i64 + 1),
                json!(h.from),
                json!(h.depth),
                json!(h.rel),
                json!(h.id),
                json!(h.kind),
                json!(h.name),
                json!(h.file),
                json!(h.line),
                json!(h.count),
                h.sites.clone(),
            ]
        })
        .collect();

    let mut notes = Vec::new();
    if !complete {
        notes.push(format!("the walk stopped after {MAX_VISITED} nodes: the answer is incomplete"));
    }
    if root.kind == "MODULE" {
        notes.push("MODULE nodes only have derived DEPENDS_ON edges: use symbol_detail".to_string());
    }
    Ok(Doc {
        head: vec![
            ("id", json!(root.id)),
            ("kind", json!(root.kind)),
            ("name", json!(root.name)),
            ("file", json!(root.file)),
            ("direction", json!(direction)),
            ("edges", json!(edges)),
            ("depth", json!(depth)),
            ("levels", json!(levels)),
            ("complete", json!(complete)),
        ],
        tables: vec![Table { name: "nodes", columns: TRACE_COLUMNS, rows, total }],
        notes,
    })
}

// ------------------------------------------------------------------------------------------
// read_source
// ------------------------------------------------------------------------------------------

const SOURCE_TABLE: &[TableSpec] = &[TableSpec { name: "lines", columns: &[col("line", "string")] }];

fn read_source_def() -> ToolDef {
    ToolDef {
        name: "read_source",
        title: "Read source lines",
        description: "Lines start..end (1-based, at most 400) of an indexed file, each as 'N: text'. \
Long lines are clipped. If the file changed since indexing the answer says so, and the line numbers of \
earlier answers may no longer fit.",
        params: vec![
            Param::text("path", 1, PATH_MAX, "File path relative to the project root."),
            Param::int("start", 1, MAX_LINE_NUMBER, "First line to read."),
            Param::int("end", 1, MAX_LINE_NUMBER, "Last line to read; at most 400 lines after start."),
        ],
        paginated: true,
        head: &[
            Field { name: "path", ty: "string" },
            Field { name: "start", ty: "integer" },
            Field { name: "end", ty: "integer" },
            Field { name: "lines_in_file", ty: "integer" },
        ],
        tables: SOURCE_TABLE,
        run: read_source,
    }
}

fn read_source(ctx: &Ctx, args: &Args) -> Result<Doc, ToolError> {
    let path = normalize_path(args.text("path"))?;
    let (start, end) = (args.int("start"), args.int("end"));
    if end < start {
        return Err(ToolError::Invalid(format!("end ({end}) is before start ({start})")));
    }
    if end - start + 1 > MAX_READ_LINES {
        return Err(ToolError::Invalid(format!(
            "{} lines requested; at most {MAX_READ_LINES} per call, so read the range in pieces",
            end - start + 1
        )));
    }
    // Only files in the index are ever read, so no path can reach anything else.
    let known = count(ctx.db, "SELECT COUNT(*) FROM file_manifest WHERE file_path = ?", vec![text(&path)])?;
    if known == 0 {
        return Err(not_found_file(ctx.db, &path));
    }
    let bytes = std::fs::read(ctx.root.join(&path))
        .map_err(|e| ToolError::NotFound { what: format!("`{}` cannot be read: {e}", echo(&path)), hints: vec![] })?;
    let mut notes = Vec::new();
    let content = match std::str::from_utf8(&bytes) {
        Ok(text) => text.to_string(),
        Err(_) => {
            notes.push("the file is not valid UTF-8: invalid bytes were replaced".to_string());
            String::from_utf8_lossy(&bytes).into_owned()
        }
    };
    let mut lines: Vec<&str> = content.split('\n').collect();
    if content.is_empty() || content.ends_with('\n') {
        lines.pop();
    }
    let in_file = lines.len() as i64;
    if start > in_file {
        return Err(ToolError::Invalid(format!("start ({start}) is past the end of the file ({in_file} lines)")));
    }
    let last = end.min(in_file);
    if last < end {
        notes.push(format!("the file has {in_file} lines, so the range ends at {last}"));
    }
    let range = (last - start + 1) as usize;
    let mut replaced = 0;
    let rows: Vec<Vec<Value>> = lines
        .iter()
        .enumerate()
        .skip(start as usize - 1 + ctx.offset as usize)
        .take((ctx.window as usize).min(range.saturating_sub(ctx.offset as usize)))
        .map(|(index, line)| {
            let line = line.strip_suffix('\r').unwrap_or(line);
            let clean: String = line
                .chars()
                .map(|c| {
                    if c.is_control() && c != '\t' {
                        replaced += 1;
                        '\u{fffd}'
                    } else {
                        c
                    }
                })
                .collect();
            vec![json!(format!("{}: {clean}", index + 1))]
        })
        .collect();
    if replaced > 0 {
        notes.push(format!("{replaced} control character(s) were replaced by U+FFFD"));
    }
    Ok(Doc {
        head: vec![
            ("path", json!(path)),
            ("start", json!(start)),
            ("end", json!(last)),
            ("lines_in_file", json!(in_file)),
        ],
        tables: vec![Table { name: "lines", columns: SOURCE_TABLE[0].columns, rows, total: range as i64 }],
        notes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_cleaned_and_dangerous_ones_refused() {
        assert_eq!(normalize_path("./src//a.rs").unwrap(), "src/a.rs");
        assert_eq!(normalize_path("src\\b\\c.rs").unwrap(), "src/b/c.rs");
        assert_eq!(normalize_path("src/dir/").unwrap(), "src/dir");
        assert_eq!(normalize_path("././x").unwrap(), "x");
        for bad in ["/etc/passwd", "C:/windows", "c:\\x", "../x", "a/../../b", "a/..", "..", "", "./", "/"] {
            assert!(normalize_path(bad).is_err(), "{bad:?} must be refused");
        }
        assert_eq!(clean_relative("src/").unwrap(), "src/", "a prefix keeps its trailing slash");
        assert!(clean_relative("..\\x").is_err());
    }

    #[test]
    fn like_wildcards_are_escaped() {
        assert_eq!(escape_like("a%b_c\\d"), "a\\%b\\_c\\\\d");
        assert_eq!(escape_like("plain"), "plain");
    }

    #[test]
    fn sites_keep_the_first_few_lines_only() {
        assert_eq!(sites(&json!("[3,5,8,13,21,34,55]")), json!([3, 5, 8, 13, 21]));
        assert_eq!(sites(&json!("[]")), json!([]));
        assert_eq!(sites(&Value::Null), json!([]));
        assert_eq!(sites(&json!("not json")), json!([]));
    }
}
