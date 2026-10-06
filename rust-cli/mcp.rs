//! A Model Context Protocol server over stdio (JSON-RPC 2.0, one message per line).
//!
//! The rules that keep answers usable by an AI agent live here, in one place, and no tool can
//! opt out of them:
//!
//! * every request is validated against the same JSON Schema that `tools/list` publishes;
//! * every answer is cut to a size budget by the renderer, never by the tool, and says how much
//!   was left out and how to continue (`shown`, `total`, `next_offset`);
//! * every answer is validated against its own published output schema before it is sent;
//! * every file named in an answer is checked against the hash recorded when it was indexed, and
//!   changed files are reported;
//! * the database is opened read-only, so no tool can change the index.

use crate::config;
use crate::db::SCHEMA_VERSION;
use crate::schema;
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::io::{BufRead, Read, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const SERVER_NAME: &str = "codebase-context-graph";
/// Newest first.
pub const SUPPORTED_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];
pub const MAX_REQUEST_BYTES: usize = 1 << 20;

/// An answer's size limit is stated in tokens, one token being about four bytes of text.
pub const BYTES_PER_TOKEN: usize = 4;
pub const MIN_BUDGET_TOKENS: i64 = 1000;
pub const DEFAULT_BUDGET_TOKENS: i64 = 1500;
pub const MAX_BUDGET_TOKENS: i64 = 3000;
pub const MAX_OFFSET: i64 = 10_000_000;
/// The most rows a tool may fetch for one call. The renderer then fits them to the budget.
pub const WINDOW: i64 = 300;

/// The longest id and path a caller may pass in, in characters. They match the output limits
/// below, so any id or path that is shown whole can be passed back.
pub const ID_MAX: usize = 640;
pub const PATH_MAX: usize = 480;

/// Output limits per value, in bytes of its JSON form (quotes and escapes included), so that a
/// row's size is bounded whatever characters it holds. Ids and paths are the handles other calls
/// need and get larger limits; a value cut at its limit ends in "…" and is counted in `clipped`.
pub const ID_CLIP: usize = 640;
pub const PATH_CLIP: usize = 480;
pub const CELL_CLIP: usize = 200;
pub const LINE_CLIP: usize = 520;
pub const SHORT_CLIP: usize = 64;
const NOTE_CLIP: usize = 300;

const MAX_NOTES: usize = 6;
const MAX_STALE_LISTED: usize = 5;
const MAX_FRESHNESS_FILES: usize = 200;
const MAX_ERROR_BYTES: usize = 1500;
const ECHO_CHARS: usize = 60;
/// Room kept for the digits of `shown` / `next_offset`, which are not known before fitting.
const DIGITS_RESERVE: usize = 24;

// ------------------------------------------------------------------------------------------
// Declarations: what a tool accepts and what it returns.
// ------------------------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub enum Kind {
    Text { min: usize, max: usize },
    Int { min: i64, max: i64 },
    Choice(&'static [&'static str]),
}

#[derive(Clone, Debug)]
pub struct Param {
    pub name: &'static str,
    pub kind: Kind,
    pub required: bool,
    pub default: Option<Value>,
    pub help: &'static str,
}

impl Param {
    pub fn text(name: &'static str, min: usize, max: usize, help: &'static str) -> Param {
        Param { name, kind: Kind::Text { min, max }, required: true, default: None, help }
    }
    pub fn int(name: &'static str, min: i64, max: i64, help: &'static str) -> Param {
        Param { name, kind: Kind::Int { min, max }, required: true, default: None, help }
    }
    pub fn choice(name: &'static str, values: &'static [&'static str], help: &'static str) -> Param {
        Param { name, kind: Kind::Choice(values), required: true, default: None, help }
    }
    /// May be left out, and then has no value.
    pub fn optional(mut self) -> Param {
        self.required = false;
        self
    }
    /// May be left out, and then has this value.
    pub fn default(mut self, value: impl Into<Value>) -> Param {
        self.required = false;
        self.default = Some(value.into());
        self
    }

    fn schema(&self) -> Value {
        let mut schema = match &self.kind {
            Kind::Text { min, max } => json!({"type": "string", "minLength": min, "maxLength": max}),
            Kind::Int { min, max } => json!({"type": "integer", "minimum": min, "maximum": max}),
            Kind::Choice(values) => json!({"type": "string", "enum": values}),
        };
        schema["description"] = json!(self.help);
        if let Some(default) = &self.default {
            schema["default"] = default.clone();
        }
        schema
    }
}

/// A top-level field of an answer, with its JSON type(s), e.g. `"string|null"`.
pub struct Field {
    pub name: &'static str,
    pub ty: &'static str,
}

/// A column of a table, with the JSON type(s) its cells must have.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Column {
    pub name: &'static str,
    pub ty: &'static str,
}

pub const fn col(name: &'static str, ty: &'static str) -> Column {
    Column { name, ty }
}

/// A table in an answer: its columns, names and types, are fixed by the declaration.
pub struct TableSpec {
    pub name: &'static str,
    pub columns: &'static [Column],
}

pub struct ToolDef {
    pub name: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub params: Vec<Param>,
    /// A paginated tool has exactly one table and accepts `offset`.
    pub paginated: bool,
    pub head: &'static [Field],
    pub tables: &'static [TableSpec],
    pub run: fn(&Ctx, &Args) -> Result<Doc, ToolError>,
}

const RESERVED_FIELDS: &[&str] = &["as_of", "notes", "stale_files", "clipped"];

impl ToolDef {
    /// The declared parameters plus the ones every tool gets from the framework.
    pub fn all_params(&self) -> Vec<Param> {
        let mut params = self.params.clone();
        if self.paginated {
            params.push(
                Param::int("offset", 0, MAX_OFFSET, "Rows to skip; pass the previous answer's next_offset.")
                    .default(0),
            );
        }
        params.push(
            Param::int(
                "budget_tokens",
                MIN_BUDGET_TOKENS,
                MAX_BUDGET_TOKENS,
                "Size limit for the answer, about 4 bytes per token.",
            )
            .default(DEFAULT_BUDGET_TOKENS),
        );
        params
    }

    pub fn input_schema(&self) -> Value {
        let params = self.all_params();
        let properties: Map<String, Value> =
            params.iter().map(|p| (p.name.to_string(), p.schema())).collect();
        let required: Vec<&str> = params.iter().filter(|p| p.required).map(|p| p.name).collect();
        json!({
            "type": "object",
            "properties": properties,
            "required": required,
            "additionalProperties": false
        })
    }

    pub fn output_schema(&self) -> Value {
        let mut properties = Map::new();
        let mut required = vec!["as_of".to_string()];
        properties.insert("as_of".into(), json!({"type": "string"}));
        for field in self.head {
            properties.insert(field.name.to_string(), type_schema(field.ty));
            required.push(field.name.to_string());
        }
        for table in self.tables {
            let width = table.columns.len();
            let cells: Vec<Value> = table.columns.iter().map(|c| type_schema(c.ty)).collect();
            properties.insert(
                table.name.to_string(),
                json!({
                    "type": "object",
                    "properties": {
                        "columns": {"type": "array", "items": {"type": "string"}},
                        "rows": {
                            "type": "array",
                            "items": {
                                "type": "array",
                                "prefixItems": cells,
                                "minItems": width,
                                "maxItems": width
                            }
                        },
                        "shown": {"type": "integer", "minimum": 0},
                        "total": {"type": "integer", "minimum": 0},
                        "next_offset": {"type": ["integer", "null"], "minimum": 0}
                    },
                    "required": ["columns", "rows", "shown", "total", "next_offset"],
                    "additionalProperties": false
                }),
            );
            required.push(table.name.to_string());
        }
        properties.insert("notes".into(), json!({"type": "array", "items": {"type": "string"}}));
        properties.insert("stale_files".into(), json!({"type": "array", "items": {"type": "string"}}));
        properties.insert("clipped".into(), json!({"type": "integer", "minimum": 1}));
        json!({
            "type": "object",
            "properties": properties,
            "required": required,
            "additionalProperties": false
        })
    }

    /// Mistakes in a tool's own declaration; the tests run this on every registered tool.
    pub fn declaration_problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        let valid_name = !self.name.is_empty()
            && self.name.len() <= 64
            && self.name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
        if !valid_name {
            problems.push(format!("{}: not a valid tool name", self.name));
        }
        if self.description.is_empty() || self.description.len() > 600 {
            problems.push(format!("{}: description must be 1-600 bytes", self.name));
        }
        if self.paginated && self.tables.len() != 1 {
            problems.push(format!("{}: a paginated tool has exactly one table", self.name));
        }
        for param in &self.params {
            if matches!(param.name, "offset" | "budget_tokens") {
                problems.push(format!("{}: `{}` is added by the framework", self.name, param.name));
            }
            if param.help.is_empty() {
                problems.push(format!("{}: parameter `{}` has no help text", self.name, param.name));
            }
            let bad_default = param.default.as_ref().filter(|d| schema::validate(&param.schema(), d).is_err());
            if bad_default.is_some() {
                problems.push(format!("{}: default of `{}` breaks its own schema", self.name, param.name));
            }
        }
        let mut seen = RESERVED_FIELDS.to_vec();
        for name in self.head.iter().map(|f| f.name).chain(self.tables.iter().map(|t| t.name)) {
            if seen.contains(&name) {
                problems.push(format!("{}: `{name}` is reserved or declared twice", self.name));
            }
            seen.push(name);
        }
        if let Err(why) = schema::lint(&self.input_schema()) {
            problems.push(format!("{}: input schema: {why}", self.name));
        }
        if let Err(why) = schema::lint(&self.output_schema()) {
            problems.push(format!("{}: output schema: {why}", self.name));
        }
        problems
    }
}

fn type_schema(ty: &str) -> Value {
    let names: Vec<&str> = ty.split('|').collect();
    if names.len() == 1 { json!({"type": names[0]}) } else { json!({"type": names}) }
}

/// Validated arguments with defaults filled in. The accessors panic on a name the tool did not
/// declare; the tests exercise every tool, so that mistake cannot ship.
pub struct Args(Map<String, Value>);

impl Args {
    pub fn text(&self, name: &str) -> &str {
        self.0.get(name).and_then(Value::as_str).unwrap_or_else(|| {
            panic!("`{name}` was not declared required or defaulted")
        })
    }
    pub fn opt_text(&self, name: &str) -> Option<&str> {
        self.0.get(name).and_then(Value::as_str)
    }
    pub fn int(&self, name: &str) -> i64 {
        self.0.get(name).and_then(Value::as_i64).unwrap_or_else(|| {
            panic!("`{name}` was not declared required or defaulted")
        })
    }
    pub fn choice(&self, name: &str) -> &str {
        self.text(name)
    }
    pub fn opt_choice(&self, name: &str) -> Option<&str> {
        self.opt_text(name)
    }
}

pub struct Ctx<'a> {
    pub db: &'a Connection,
    pub root: &'a Path,
    pub offset: i64,
    pub window: i64,
}

pub struct Table {
    pub name: &'static str,
    pub columns: &'static [Column],
    /// At most `Ctx::window` rows, starting at `Ctx::offset` (paginated tools) or at 0.
    pub rows: Vec<Vec<Value>>,
    /// How many rows exist in all, not how many are in `rows`.
    pub total: i64,
}

pub struct Doc {
    pub head: Vec<(&'static str, Value)>,
    pub tables: Vec<Table>,
    pub notes: Vec<String>,
}

#[derive(Debug)]
pub enum ToolError {
    /// The request was well-formed JSON but cannot be answered as asked.
    Invalid(String),
    NotFound { what: String, hints: Vec<String> },
    /// A bug. The conformance tests exist to make sure this never happens.
    Internal(String),
}

impl ToolError {
    pub fn text(&self) -> String {
        let text = match self {
            ToolError::Invalid(why) => format!("invalid request: {why}"),
            ToolError::NotFound { what, hints } if hints.is_empty() => format!("not found: {what}"),
            ToolError::NotFound { what, hints } => {
                format!("not found: {what}\nhint: {}", hints.join(" | "))
            }
            ToolError::Internal(why) => {
                format!("internal error: {why} (this is a bug in {SERVER_NAME})")
            }
        };
        clip_json(&text, MAX_ERROR_BYTES).unwrap_or(text)
    }
}

// ------------------------------------------------------------------------------------------
// Rendering: the only place an answer is sized.
// ------------------------------------------------------------------------------------------

/// A caller's own text, shortened for quoting in a message about it.
pub fn echo(text: &str) -> String {
    clip_chars(text, ECHO_CHARS)
}

fn clip_chars(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let head: String = text.chars().take(limit.saturating_sub(1)).collect();
    format!("{head}…")
}

/// Bytes a character takes inside a JSON string.
fn json_len(c: char) -> usize {
    match c {
        '"' | '\\' | '\n' | '\r' | '\t' | '\u{8}' | '\u{c}' => 2,
        c if (c as u32) < 0x20 => 6,
        c => c.len_utf8(),
    }
}

/// `None` if the JSON form of `text` (quotes included) is at most `limit` bytes; otherwise the
/// longest prefix that, followed by "…", is.
pub fn clip_json(text: &str, limit: usize) -> Option<String> {
    let total: usize = 2 + text.chars().map(json_len).sum::<usize>();
    if total <= limit {
        return None;
    }
    let mut used = 2 + '…'.len_utf8();
    let mut out = String::new();
    for c in text.chars() {
        let cost = json_len(c);
        if used + cost > limit {
            break;
        }
        used += cost;
        out.push(c);
    }
    out.push('…');
    Some(out)
}

#[derive(Default)]
struct Clips {
    cells: usize,
    handles: usize,
}

fn limit_for(column: &str) -> (usize, bool) {
    match column {
        "id" | "container" => (ID_CLIP, true), // node ids, which other calls need
        "file" | "path" => (PATH_CLIP, true),
        "line" => (LINE_CLIP, false), // a line of source code
        // Short labels (a kind, a relation, a state...) never need more.
        "kind" | "rel" | "direction" | "edges" | "language" | "status" | "state" | "indexed_by" | "type" => {
            (SHORT_CLIP, false)
        }
        _ => (CELL_CLIP, false),
    }
}

fn clip_value(value: &mut Value, column: &str, clips: &mut Clips) {
    let (limit, handle) = limit_for(column);
    let Value::String(text) = value else { return };
    let Some(clipped) = clip_json(text, limit) else { return };
    *text = clipped;
    if handle {
        clips.handles += 1;
    } else {
        clips.cells += 1;
    }
}

fn row_len(row: &[Value]) -> usize {
    // The JSON array of the row plus the comma that separates it from the next one.
    serde_json::to_string(row).map(|s| s.len()).unwrap_or(usize::MAX / 4) + 1
}

/// How many rows of each table fit in `pool` bytes: a fair share first, then what is left over.
fn fit(tables: &[Table], pool: usize) -> Vec<usize> {
    let mut pool = pool;
    let mut shown = Vec::with_capacity(tables.len());
    for (index, table) in tables.iter().enumerate() {
        let share = pool / (tables.len() - index);
        let (mut used, mut count) = (0, 0);
        for row in &table.rows {
            let cost = row_len(row);
            if used + cost > share {
                break;
            }
            used += cost;
            count += 1;
        }
        pool -= used;
        shown.push(count);
    }
    for (index, table) in tables.iter().enumerate() {
        while shown[index] < table.rows.len() {
            let cost = row_len(&table.rows[shown[index]]);
            if cost > pool {
                break;
            }
            pool -= cost;
            shown[index] += 1;
        }
    }
    shown
}

/// Everything that goes into an answer except how many rows of each table fit.
struct Parts<'a> {
    def: &'a ToolDef,
    doc: &'a Doc,
    offset: i64,
    as_of: &'a str,
    stale: &'a [String],
    clips: &'a Clips,
}

fn assemble(parts: &Parts, shown: &[usize], notes: &[String]) -> Value {
    let Parts { def, doc, offset, as_of, stale, clips } = parts;
    let mut out = Map::new();
    out.insert("as_of".into(), json!(as_of));
    for (name, value) in &doc.head {
        out.insert((*name).to_string(), value.clone());
    }
    for (table, count) in doc.tables.iter().zip(shown) {
        let rows: Vec<&Vec<Value>> = table.rows.iter().take(*count).collect();
        let end = offset + *count as i64;
        let next = if def.paginated && end < table.total { json!(end) } else { Value::Null };
        out.insert(
            table.name.to_string(),
            json!({
                "columns": table.columns.iter().map(|c| c.name).collect::<Vec<_>>(),
                "rows": rows,
                "shown": count,
                "total": table.total,
                "next_offset": next
            }),
        );
    }
    if !notes.is_empty() {
        out.insert("notes".into(), json!(notes));
    }
    if !stale.is_empty() {
        out.insert("stale_files".into(), json!(stale));
    }
    if clips.cells + clips.handles > 0 {
        out.insert("clipped".into(), json!(clips.cells + clips.handles));
    }
    Value::Object(out)
}

/// Checks that a tool returned exactly what it declared, and no more than it was allowed to.
fn check_declared(def: &ToolDef, doc: &Doc, offset: i64) -> Result<(), ToolError> {
    for (name, _) in &doc.head {
        if !def.head.iter().any(|f| f.name == *name) {
            return Err(ToolError::Internal(format!("undeclared field `{name}`")));
        }
    }
    for field in def.head {
        if !doc.head.iter().any(|(n, _)| *n == field.name) {
            return Err(ToolError::Internal(format!("declared field `{}` is missing", field.name)));
        }
    }
    if doc.tables.len() != def.tables.len() {
        return Err(ToolError::Internal("tables do not match the declaration".into()));
    }
    for (table, spec) in doc.tables.iter().zip(def.tables) {
        if table.name != spec.name || table.columns != spec.columns {
            return Err(ToolError::Internal(format!("table `{}` does not match its declaration", table.name)));
        }
        if table.rows.iter().any(|r| r.len() != spec.columns.len()) {
            return Err(ToolError::Internal(format!("a row of `{}` has the wrong width", table.name)));
        }
        if table.rows.len() as i64 > WINDOW {
            return Err(ToolError::Internal(format!("`{}` returned more than {WINDOW} rows", table.name)));
        }
        let start = if def.paginated { offset } else { 0 };
        if !table.rows.is_empty() && table.total < start + table.rows.len() as i64 {
            return Err(ToolError::Internal(format!(
                "`{}` reports fewer rows in total than it returned",
                table.name
            )));
        }
    }
    Ok(())
}

/// Turns a tool's rows into an answer of at most `budget` bytes, or explains why that is impossible.
pub fn render(
    def: &ToolDef,
    mut doc: Doc,
    offset: i64,
    budget: usize,
    as_of: &str,
    stale: &[(String, &'static str)],
    extra_notes: Vec<String>,
) -> Result<Value, ToolError> {
    check_declared(def, &doc, offset)?;

    let mut clips = Clips::default();
    for (name, value) in doc.head.iter_mut() {
        clip_value(value, name, &mut clips);
    }
    for table in doc.tables.iter_mut() {
        for row in table.rows.iter_mut() {
            for (cell, column) in row.iter_mut().zip(table.columns) {
                clip_value(cell, column.name, &mut clips);
            }
        }
    }

    let mut notes: Vec<String> = doc.notes.iter().chain(extra_notes.iter()).cloned().collect();
    let mut stale_listed: Vec<String> =
        stale.iter().map(|(p, _)| clip_json(p, PATH_CLIP).unwrap_or_else(|| p.clone())).collect();
    if !stale.is_empty() {
        let missing = stale.iter().filter(|(_, state)| *state == "missing").count();
        notes.insert(
            0,
            format!(
                "{} file(s) changed and {missing} missing since indexing (stale_files): line numbers \
                 for them may be wrong; run `{SERVER_NAME} index` again",
                stale.len() - missing
            ),
        );
        stale_listed.truncate(MAX_STALE_LISTED);
    }
    if clips.handles > 0 {
        notes.push(format!(
            "{} id or path value(s) are too long to show whole and are clipped; \
             a clipped id cannot be passed to other tools",
            clips.handles
        ));
    }
    let past_the_end = doc
        .tables
        .first()
        .filter(|t| def.paginated && t.rows.is_empty() && offset > 0 && offset >= t.total);
    if let Some(table) = past_the_end {
        notes.push(format!("offset {offset} is past the last row ({} rows in all)", table.total));
    }
    if notes.len() > MAX_NOTES {
        let hidden = notes.len() - (MAX_NOTES - 1);
        notes.truncate(MAX_NOTES - 1);
        notes.push(format!("(+{hidden} more notes)"));
    }
    let notes: Vec<String> = notes.into_iter().map(|n| clip_json(&n, NOTE_CLIP).unwrap_or(n)).collect();

    // Fit the rows, then add the notes that depend on the fit. If those pushed the answer over
    // the budget, leave more room and fit again. Optional parts (the stale-file list, notes) give
    // way before the answer would overflow; the count of what was left out is always stated.
    let mut notes = notes;
    let mut omitted = 0usize;
    let mut extra_reserve = 0usize;
    for _attempt in 0..(8 + MAX_NOTES + MAX_STALE_LISTED) {
        let parts = Parts { def, doc: &doc, offset, as_of, stale: &stale_listed, clips: &clips };
        let shown_notes = with_omitted(&notes, omitted);
        let skeleton = assemble(&parts, &vec![0; doc.tables.len()], &shown_notes);
        let overhead = skeleton.to_string().len() + DIGITS_RESERVE * doc.tables.len() + extra_reserve;
        if overhead > budget {
            if !notes.is_empty() {
                notes.pop();
                omitted += 1;
            } else if !stale_listed.is_empty() {
                stale_listed.pop();
            } else {
                return Err(ToolError::Internal(format!(
                    "the answer's fixed part ({overhead} bytes) does not fit the budget ({budget} bytes)"
                )));
            }
            continue;
        }
        let shown = fit(&doc.tables, budget - overhead);

        // A paginated answer must show at least one row, or paging could never advance. Notes and
        // the stale-file list give way to that before it is called impossible.
        if def.paginated && doc.tables.iter().zip(&shown).any(|(t, n)| *n == 0 && !t.rows.is_empty()) {
            if !notes.is_empty() {
                notes.pop();
                omitted += 1;
            } else if !stale_listed.is_empty() {
                stale_listed.pop();
            } else {
                return Err(ToolError::Internal(
                    "a row does not fit the budget, so paging could not make progress".into(),
                ));
            }
            continue;
        }

        let mut fitted_notes = shown_notes.clone();
        for ((table, spec), count) in doc.tables.iter().zip(def.tables).zip(&shown) {
            if !def.paginated && (*count as i64) < table.total {
                fitted_notes.push(format!(
                    "`{}` shows {count} of {} rows; raise budget_tokens (at most {MAX_BUDGET_TOKENS}) to see more",
                    spec.name, table.total
                ));
            }
        }
        let value = assemble(&parts, &shown, &fitted_notes);
        let size = value.to_string().len();
        if size <= budget {
            if let Err(problems) = schema::validate(&def.output_schema(), &value) {
                return Err(ToolError::Internal(format!(
                    "the answer breaks its own output schema: {}",
                    problems.join("; ")
                )));
            }
            return Ok(value);
        }
        extra_reserve += size - budget + 8;
    }
    Err(ToolError::Internal("could not fit the answer to the budget".into()))
}

fn with_omitted(notes: &[String], omitted: usize) -> Vec<String> {
    let mut all = notes.to_vec();
    if omitted > 0 {
        all.push(format!("(+{omitted} notes left out for space)"));
    }
    all
}

// ------------------------------------------------------------------------------------------
// Freshness: has a file changed since it was indexed?
// ------------------------------------------------------------------------------------------

pub fn file_hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// The files among `paths` that are in the index but whose content on disk is no longer what was
/// indexed. A path that is not in the index is ignored: nothing outside the manifest is ever read.
pub fn file_states(db: &Connection, root: &Path, paths: &[String]) -> Vec<(String, &'static str)> {
    let mut states = Vec::new();
    let Ok(mut lookup) = db.prepare_cached("SELECT content_hash FROM file_manifest WHERE file_path = ?1")
    else {
        return states;
    };
    for path in paths.iter().take(MAX_FRESHNESS_FILES) {
        let Ok(Some(expected)) = lookup.query_row([path], |r| r.get::<_, String>(0)).optional() else {
            continue;
        };
        match std::fs::read(root.join(path)) {
            Ok(bytes) if file_hash(&bytes) == expected => {}
            Ok(_) => states.push((path.clone(), "changed")),
            Err(_) => states.push((path.clone(), "missing")),
        }
    }
    states
}

/// Every file path named in an answer, found by column or field name so a tool cannot forget one.
fn files_named(doc: &Doc) -> Vec<String> {
    let mut files: Vec<String> = Vec::new();
    for (name, value) in &doc.head {
        if let Some(text) = value.as_str().filter(|_| matches!(*name, "file" | "path")) {
            files.push(text.to_string());
        }
    }
    for table in &doc.tables {
        for (index, column) in table.columns.iter().enumerate() {
            if matches!(column.name, "file" | "path") {
                files.extend(table.rows.iter().filter_map(|r| r.get(index)?.as_str()).map(String::from));
            }
        }
    }
    files.sort();
    files.dedup();
    files
}

// ------------------------------------------------------------------------------------------
// The database.
// ------------------------------------------------------------------------------------------

pub fn open_read_only(path: &Path) -> Result<Connection, rusqlite::Error> {
    let db = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    db.pragma_update(None, "query_only", true)?;
    db.busy_timeout(Duration::from_millis(2000))?;
    Ok(db)
}

#[cfg(unix)]
fn identity(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).ok().map(|m| (m.dev(), m.ino()))
}

#[cfg(not(unix))]
fn identity(_path: &Path) -> Option<(u64, u64)> {
    None
}

// ------------------------------------------------------------------------------------------
// The server.
// ------------------------------------------------------------------------------------------

#[derive(Debug, PartialEq)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

fn rpc(code: i64, message: impl Into<String>) -> RpcError {
    RpcError { code, message: message.into() }
}

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const NOT_INITIALIZED: i64 = -32002;

const INSTRUCTIONS: &str = "Code graph of one project, built from compiler-grade SCIP indexes. \
Start with `overview` (coverage and freshness). Find code with `find_symbols`, list a file with \
`file_outline`, inspect one symbol with `symbol_detail`, follow callers or dependencies with `trace`, \
and read exact lines with `read_source`. Every answer is size-limited (`budget_tokens`): when \
`next_offset` is not null there is more, so repeat the call with `offset` set to it. Lines are \
1-based. If an answer lists `stale_files`, those files changed after indexing.";

pub struct Server {
    root: PathBuf,
    tools: Vec<ToolDef>,
    db: Option<(Connection, Option<(u64, u64)>)>,
    version: Option<&'static str>,
}

impl Server {
    pub fn new(root: &Path, tools: Vec<ToolDef>) -> Server {
        Server { root: root.to_path_buf(), tools, db: None, version: None }
    }

    fn at_least(&self, version: &str) -> bool {
        self.version.is_some_and(|v| v >= version)
    }

    fn ensure_db(&mut self) -> Result<(), ToolError> {
        let path = config::database_path(&self.root);
        let current = identity(&path);
        if let Some((_, opened)) = &self.db {
            if *opened == current && path.exists() {
                return Ok(());
            }
            self.db = None; // the index was deleted or replaced: reopen
        }
        if !path.exists() {
            return Err(ToolError::NotFound {
                what: format!("no index at {}", path.display()),
                hints: vec![format!("run `{SERVER_NAME} index --project-root {}`", self.root.display())],
            });
        }
        let db = open_read_only(&path)
            .map_err(|e| ToolError::Internal(format!("cannot open the index read-only: {e}")))?;
        let version: i64 = db
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .map_err(|e| ToolError::Internal(format!("cannot read the index version: {e}")))?;
        if version != SCHEMA_VERSION {
            return Err(ToolError::Invalid(format!(
                "the index has layout version {version}, this server reads {SCHEMA_VERSION}: \
                 run `{SERVER_NAME} index` again"
            )));
        }
        self.db = Some((db, current));
        Ok(())
    }

    /// One line of input, which may hold a request, a notification or a batch.
    pub fn handle_line(&mut self, line: &[u8]) -> Option<Value> {
        if line.iter().all(|b| b.is_ascii_whitespace()) {
            return None;
        }
        match serde_json::from_slice::<Value>(line) {
            Ok(message) => self.handle_message(message),
            Err(e) => Some(error_reply(&Value::Null, rpc(PARSE_ERROR, format!("parse error: {e}")))),
        }
    }

    pub fn handle_message(&mut self, message: Value) -> Option<Value> {
        match message {
            Value::Array(batch) if batch.is_empty() => {
                Some(error_reply(&Value::Null, rpc(INVALID_REQUEST, "empty batch")))
            }
            Value::Array(batch) => {
                let replies: Vec<Value> = batch.into_iter().filter_map(|m| self.handle_single(m)).collect();
                if replies.is_empty() { None } else { Some(Value::Array(replies)) }
            }
            other => self.handle_single(other),
        }
    }

    fn handle_single(&mut self, message: Value) -> Option<Value> {
        let Some(object) = message.as_object() else {
            return Some(error_reply(&Value::Null, rpc(INVALID_REQUEST, "a message must be a JSON object")));
        };
        let id = object.get("id").cloned();
        let reply_id = id.clone().unwrap_or(Value::Null);
        // A response to a request this server never sent: nothing to do.
        if object.get("method").is_none() && (object.contains_key("result") || object.contains_key("error")) {
            return None;
        }
        if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
            return id.map(|_| error_reply(&reply_id, rpc(INVALID_REQUEST, "`jsonrpc` must be \"2.0\"")));
        }
        let Some(method) = object.get("method").and_then(Value::as_str) else {
            return Some(error_reply(&reply_id, rpc(INVALID_REQUEST, "`method` must be a string")));
        };
        if id.as_ref().is_some_and(|id| !(id.is_string() || id.is_number())) {
            return Some(error_reply(&Value::Null, rpc(INVALID_REQUEST, "`id` must be a string or a number")));
        }
        let params = object.get("params").cloned().unwrap_or(Value::Null);
        let outcome = self.dispatch(method, params, id.is_some());
        match (id, outcome) {
            (Some(id), Ok(result)) => Some(json!({"jsonrpc": "2.0", "id": id, "result": result})),
            (Some(id), Err(error)) => Some(error_reply(&id, error)),
            (None, _) => None, // notifications never get a reply
        }
    }

    fn dispatch(&mut self, method: &str, params: Value, is_request: bool) -> Result<Value, RpcError> {
        match method {
            "ping" => Ok(json!({})),
            "initialize" if is_request => self.initialize(&params),
            "tools/list" if is_request => {
                self.require_initialized()?;
                if params.get("cursor").is_some() {
                    return Err(rpc(INVALID_PARAMS, "unknown cursor: this server lists all tools in one page"));
                }
                Ok(json!({"tools": self.tool_definitions()}))
            }
            "tools/call" if is_request => {
                self.require_initialized()?;
                self.tools_call(&params)
            }
            _ if is_request => Err(rpc(METHOD_NOT_FOUND, format!("method not found: {method}"))),
            _ => Ok(Value::Null), // initialized, cancelled, progress...: nothing to do
        }
    }

    fn require_initialized(&self) -> Result<(), RpcError> {
        if self.version.is_none() {
            return Err(rpc(NOT_INITIALIZED, "the server is not initialized: send `initialize` first"));
        }
        Ok(())
    }

    fn initialize(&mut self, params: &Value) -> Result<Value, RpcError> {
        let Some(requested) = params.get("protocolVersion").and_then(Value::as_str) else {
            return Err(rpc(INVALID_PARAMS, "`protocolVersion` (a string) is required"));
        };
        let version = SUPPORTED_VERSIONS
            .iter()
            .copied()
            .find(|v| *v == requested)
            .unwrap_or(SUPPORTED_VERSIONS[0]);
        self.version = Some(version);
        let mut result = json!({
            "protocolVersion": version,
            "capabilities": {"tools": {"listChanged": false}},
            "serverInfo": {"name": SERVER_NAME, "version": env!("CARGO_PKG_VERSION")}
        });
        if self.at_least("2025-03-26") {
            result["instructions"] = json!(INSTRUCTIONS);
        }
        Ok(result)
    }

    fn tool_definitions(&self) -> Vec<Value> {
        self.tools
            .iter()
            .map(|def| {
                let mut tool = json!({
                    "name": def.name,
                    "description": def.description,
                    "inputSchema": def.input_schema()
                });
                if self.at_least("2025-03-26") {
                    tool["annotations"] = json!({
                        "readOnlyHint": true,
                        "destructiveHint": false,
                        "idempotentHint": true,
                        "openWorldHint": false
                    });
                }
                if self.at_least("2025-06-18") {
                    tool["title"] = json!(def.title);
                    tool["outputSchema"] = def.output_schema();
                }
                tool
            })
            .collect()
    }

    fn tools_call(&mut self, params: &Value) -> Result<Value, RpcError> {
        let Some(name) = params.get("name").and_then(Value::as_str) else {
            return Err(rpc(INVALID_PARAMS, "`name` (a string) is required"));
        };
        let arguments = match params.get("arguments") {
            None | Some(Value::Null) => json!({}),
            Some(value @ Value::Object(_)) => value.clone(),
            Some(_) => return Err(rpc(INVALID_PARAMS, "`arguments` must be an object")),
        };
        if !self.tools.iter().any(|t| t.name == name) {
            let known: Vec<&str> = self.tools.iter().map(|t| t.name).collect();
            return Err(rpc(INVALID_PARAMS, format!("unknown tool `{name}` (available: {})", known.join(", "))));
        }
        Ok(self.call_tool(name, &arguments))
    }

    /// Runs a tool and returns the MCP `tools/call` result. A failed call is a result with
    /// `isError` set, so the model can read the message and correct itself.
    pub fn call_tool(&mut self, name: &str, arguments: &Value) -> Value {
        self.call_tool_sized(name, arguments, None)
    }

    /// Tests only: fit the answer to a budget smaller than any caller may ask for, so paging can
    /// be exercised on small data.
    #[cfg(test)]
    pub fn call_tool_with_budget_bytes(&mut self, name: &str, arguments: &Value, bytes: usize) -> Value {
        self.call_tool_sized(name, arguments, Some(bytes))
    }

    fn call_tool_sized(&mut self, name: &str, arguments: &Value, budget_override: Option<usize>) -> Value {
        let structured = self.at_least("2025-06-18");
        match self.run_tool(name, arguments, budget_override) {
            Ok(value) => {
                let mut result = json!({
                    "content": [{"type": "text", "text": value.to_string()}],
                    "isError": false
                });
                if structured {
                    result["structuredContent"] = value;
                }
                result
            }
            Err(error) => json!({
                "content": [{"type": "text", "text": error.text()}],
                "isError": true
            }),
        }
    }

    fn run_tool(&mut self, name: &str, arguments: &Value, budget_override: Option<usize>) -> Result<Value, ToolError> {
        let Some(index) = self.tools.iter().position(|t| t.name == name) else {
            return Err(ToolError::Invalid(format!("unknown tool `{name}`")));
        };
        let (values, offset, budget) = {
            let def = &self.tools[index];
            let params = def.all_params();
            let problems = schema::validate(&def.input_schema(), arguments)
                .err()
                .unwrap_or_default();
            if !problems.is_empty() {
                let usage: Vec<String> = params
                    .iter()
                    .map(|p| if p.required { p.name.to_string() } else { format!("{}?", p.name) })
                    .collect();
                return Err(ToolError::Invalid(format!(
                    "{} (usage: {name}({}))",
                    problems.join("; "),
                    usage.join(", ")
                )));
            }
            let mut values = arguments.as_object().cloned().unwrap_or_default();
            for (key, value) in &values {
                if value.as_str().is_some_and(|s| s.contains('\0')) {
                    return Err(ToolError::Invalid(format!("`{key}` must not contain a NUL character")));
                }
            }
            for param in &params {
                if let (false, Some(default)) = (values.contains_key(param.name), &param.default) {
                    values.insert(param.name.to_string(), default.clone());
                }
            }
            let offset = values.get("offset").and_then(Value::as_i64).unwrap_or(0);
            let budget = values.get("budget_tokens").and_then(Value::as_i64).unwrap_or(DEFAULT_BUDGET_TOKENS);
            (values, offset, budget as usize * BYTES_PER_TOKEN)
        };

        self.ensure_db()?;
        let Some((db, _)) = self.db.as_ref() else {
            return Err(ToolError::Internal("the index is not open".into()));
        };
        let def = &self.tools[index];
        let args = Args(values);
        let ctx = Ctx { db, root: &self.root, offset, window: WINDOW };
        let doc = match catch_unwind(AssertUnwindSafe(|| (def.run)(&ctx, &args))) {
            Ok(result) => result?,
            Err(_) => return Err(ToolError::Internal(format!("tool `{name}` panicked"))),
        };

        let as_of: String = db
            .query_row("SELECT value FROM meta WHERE key = 'generated_at'", [], |r| r.get(0))
            .unwrap_or_else(|_| "unknown".to_string());
        let named = files_named(&doc);
        let stale = file_states(db, &self.root, &named);
        let mut extra = Vec::new();
        if named.len() > MAX_FRESHNESS_FILES {
            extra.push(format!("freshness was checked for the first {MAX_FRESHNESS_FILES} files only"));
        }
        render(def, doc, offset, budget_override.unwrap_or(budget), &as_of, &stale, extra)
    }
}

fn error_reply(id: &Value, error: RpcError) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": error.code, "message": error.message}})
}

// ------------------------------------------------------------------------------------------
// Transport.
// ------------------------------------------------------------------------------------------

pub enum Line {
    Eof,
    Message(Vec<u8>),
    TooLarge,
}

/// Reads one line without ever holding more than `max` bytes of it.
pub fn read_line(input: &mut impl BufRead, max: usize) -> std::io::Result<Line> {
    let mut buffer = Vec::new();
    let read = input.by_ref().take(max as u64 + 1).read_until(b'\n', &mut buffer)?;
    if read == 0 {
        return Ok(Line::Eof);
    }
    if buffer.last() == Some(&b'\n') {
        buffer.pop();
        if buffer.last() == Some(&b'\r') {
            buffer.pop();
        }
        return Ok(Line::Message(buffer));
    }
    if buffer.len() > max {
        // Throw the rest of the oversized line away, in pieces.
        loop {
            let mut sink = Vec::new();
            let n = input.by_ref().take(64 * 1024).read_until(b'\n', &mut sink)?;
            if n == 0 || sink.last() == Some(&b'\n') {
                break;
            }
        }
        return Ok(Line::TooLarge);
    }
    Ok(Line::Message(buffer)) // the last line, without a newline
}

pub fn serve(server: &mut Server, mut input: impl BufRead, mut output: impl Write) -> std::io::Result<()> {
    loop {
        let reply = match read_line(&mut input, MAX_REQUEST_BYTES)? {
            Line::Eof => return Ok(()),
            Line::Message(bytes) => server.handle_line(&bytes),
            Line::TooLarge => Some(error_reply(
                &Value::Null,
                rpc(INVALID_REQUEST, format!("request larger than {MAX_REQUEST_BYTES} bytes")),
            )),
        };
        if let Some(reply) = reply {
            writeln!(output, "{reply}")?;
            output.flush()?;
        }
    }
}

pub fn run(root: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let tools = crate::tools::registry();
    let problems: Vec<String> = tools.iter().flat_map(|t| t.declaration_problems()).collect();
    if !problems.is_empty() {
        return Err(format!("invalid tool declarations: {}", problems.join("; ")).into());
    }
    let mut server = Server::new(root, tools);
    eprintln!("{SERVER_NAME} mcp: serving {} (index: {})", root.display(), config::database_path(root).display());
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    match serve(&mut server, stdin.lock(), stdout.lock()) {
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        other => Ok(other?),
    }
}

