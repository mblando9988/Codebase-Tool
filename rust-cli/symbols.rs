//! Interpreting SCIP symbol strings.
//!
//! Indexers disagree about how much they fill in: rust-analyzer sets `kind` and
//! `display_name`, while scip-typescript and scip-python leave both empty. So the symbol
//! string itself (its descriptors: `Chart#render().` = method `render` of type `Chart`)
//! is the common ground, and `kind` refines it when the indexer provides one.

use protobuf::MessageField;
use scip::symbol::{SymbolFormatOptions, format_symbol_with, is_local_symbol, parse_symbol};
use scip::types::descriptor::Suffix;
use scip::types::symbol_information::Kind;
use scip::types::{Descriptor, Symbol, SymbolInformation};

/// Node types that represent code entities (as opposed to FILE, MODULE and EXTERNAL).
pub const SYMBOL_TYPES: &[&str] = &[
    "CLASS",
    "INTERFACE",
    "STRUCT",
    "ENUM",
    "TRAIT",
    "TYPE",
    "TYPE_ALIAS",
    "FUNCTION",
    "METHOD",
    "MACRO",
    "FIELD",
    "VARIABLE",
    "CONSTANT",
    "ENUM_MEMBER",
    "NAMESPACE",
];

pub fn is_callable(node_type: &str) -> bool {
    matches!(node_type, "FUNCTION" | "METHOD" | "MACRO")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entity {
    /// Not a graph entity: locals, parameters, type parameters, unparsable symbols.
    Skip,
    /// A language-level module or namespace. A module defined at the very start of a
    /// file *is* that file (TypeScript/Python/Rust file modules) and gets no node of its own.
    Module,
    /// A code entity with the given node type (see `SYMBOL_TYPES`).
    Symbol(&'static str),
}

#[derive(Debug, Clone)]
pub struct Parsed {
    pub entity: Entity,
    /// Stable node id: `{package manager}:{package}:{descriptors}`. The package *version*
    /// is left out on purpose: some indexers use the git revision, which would change
    /// every id on every commit.
    pub id: String,
    /// Id of the lexical owner (the type that owns a method, the module that owns a
    /// function). The owner may or may not be a node of its own.
    pub parent_id: Option<String>,
    /// `(package manager, package name)`, e.g. `("cargo", "serde")`.
    pub package: Option<(String, String)>,
    pub name: String,
    /// Type-qualified name, e.g. `Chart.render`.
    pub qualified: String,
}

fn skip() -> Parsed {
    Parsed {
        entity: Entity::Skip,
        id: String::new(),
        parent_id: None,
        package: None,
        name: String::new(),
        qualified: String::new(),
    }
}

fn suffix_of(d: &Descriptor) -> Suffix {
    d.suffix.enum_value().unwrap_or(Suffix::UnspecifiedSuffix)
}

fn descriptors_string(descriptors: &[Descriptor]) -> String {
    let mut symbol = Symbol::default();
    symbol.descriptors = descriptors.to_vec();
    symbol.package = MessageField::none();
    format_symbol_with(
        symbol,
        SymbolFormatOptions {
            include_scheme: false,
            include_package_manager: false,
            include_package_name: false,
            include_package_version: false,
            include_descriptor: true,
        },
    )
}

fn id_of(package: &Option<(String, String)>, descriptors: &[Descriptor]) -> String {
    let (manager, name) = match package {
        Some((m, n)) => (m.as_str(), n.as_str()),
        None => ("", ""),
    };
    format!(
        "{}:{}:{}",
        if manager.is_empty() { "scip" } else { manager },
        if name.is_empty() { "_" } else { name },
        descriptors_string(descriptors)
    )
}

fn type_descriptor(name: &str) -> Descriptor {
    let mut d = Descriptor::default();
    d.name = name.to_string();
    d.suffix = protobuf::EnumOrUnknown::new(Suffix::Type);
    d
}

/// Index of the `impl` descriptor in rust-analyzer's encoding of impl blocks, where
/// `impl Trait for Foo { fn bar() }` becomes `impl#[Foo][Trait]bar().`
fn impl_index(descriptors: &[Descriptor]) -> Option<usize> {
    descriptors.iter().enumerate().position(|(i, d)| {
        d.name == "impl"
            && matches!(suffix_of(d), Suffix::Type)
            && descriptors
                .get(i + 1)
                .is_some_and(|next| matches!(suffix_of(next), Suffix::TypeParameter))
    })
}

fn entity_from_kind(kind: Kind) -> Option<Entity> {
    let sym = |t: &'static str| Some(Entity::Symbol(t));
    match kind {
        Kind::Class | Kind::SingletonClass | Kind::Object | Kind::Mixin | Kind::Extension => {
            sym("CLASS")
        }
        Kind::Interface | Kind::Protocol => sym("INTERFACE"),
        Kind::Struct | Kind::Union => sym("STRUCT"),
        Kind::Enum => sym("ENUM"),
        Kind::Trait | Kind::TypeClass => sym("TRAIT"),
        Kind::TypeAlias | Kind::TypeFamily | Kind::AssociatedType | Kind::DataFamily => {
            sym("TYPE_ALIAS")
        }
        Kind::Type => sym("TYPE"),
        Kind::Function => sym("FUNCTION"),
        Kind::Method
        | Kind::Constructor
        | Kind::MethodAlias
        | Kind::MethodSpecification
        | Kind::AbstractMethod
        | Kind::StaticMethod
        | Kind::ProtocolMethod
        | Kind::PureVirtualMethod
        | Kind::TraitMethod
        | Kind::TypeClassMethod
        | Kind::SingletonMethod
        | Kind::Getter
        | Kind::Setter
        | Kind::Accessor => sym("METHOD"),
        Kind::Macro => sym("MACRO"),
        Kind::Field
        | Kind::Property
        | Kind::StaticField
        | Kind::StaticProperty
        | Kind::StaticDataMember
        | Kind::Attribute => sym("FIELD"),
        Kind::Variable | Kind::StaticVariable | Kind::Value => sym("VARIABLE"),
        Kind::Constant => sym("CONSTANT"),
        Kind::EnumMember => sym("ENUM_MEMBER"),
        Kind::Module
        | Kind::Namespace
        | Kind::Package
        | Kind::PackageObject
        | Kind::File
        | Kind::Library => Some(Entity::Module),
        Kind::Parameter
        | Kind::ParameterLabel
        | Kind::SelfParameter
        | Kind::ThisParameter
        | Kind::TypeParameter
        | Kind::MethodReceiver => Some(Entity::Skip),
        _ => None,
    }
}

/// Analyzes one SCIP symbol. `info` is the `SymbolInformation` from the document that
/// defines it, when available; its `kind` takes precedence over what the descriptors imply.
pub fn analyze(raw: &str, info: Option<&SymbolInformation>) -> Parsed {
    if raw.is_empty() || is_local_symbol(raw) {
        return skip();
    }
    let Ok(symbol) = parse_symbol(raw) else {
        return skip();
    };
    let descs = &symbol.descriptors;
    let n = descs.len();
    if n == 0 {
        return skip();
    }

    let package = symbol
        .package
        .as_ref()
        .map(|p| (p.manager.clone(), p.name.clone()));
    let impl_idx = impl_index(descs);
    let last = &descs[n - 1];

    let is_member = impl_idx.is_some()
        || (n >= 2 && matches!(suffix_of(&descs[n - 2]), Suffix::Type));
    let by_descriptor = match suffix_of(last) {
        Suffix::Parameter | Suffix::TypeParameter | Suffix::Local => Entity::Skip,
        // Python marks a module with `__init__:`. Any other meta symbol is indexer
        // bookkeeping (scip-typescript emits `plainFn0:` for `module.exports = { plainFn }`).
        Suffix::Meta if last.name == "__init__" => Entity::Module,
        Suffix::Meta => Entity::Skip,
        Suffix::Namespace | Suffix::Package => Entity::Module,
        Suffix::Macro => Entity::Symbol("MACRO"),
        Suffix::Method => Entity::Symbol(if is_member { "METHOD" } else { "FUNCTION" }),
        Suffix::Type => Entity::Symbol("TYPE"),
        Suffix::Term => Entity::Symbol(if is_member { "FIELD" } else { "VARIABLE" }),
        _ => Entity::Skip,
    };
    // Something declared inside a parameter (the properties of `props: { name: string }`) or
    // inside an indexer-generated scope is not an entity of its own. The type parameters of
    // rust-analyzer's `impl#[Foo]` encoding are part of the owner, not such a scope.
    let inside_non_entity = descs[..n - 1].iter().any(|d| match suffix_of(d) {
        Suffix::Parameter | Suffix::Local => true,
        Suffix::Meta => d.name != "__init__",
        Suffix::TypeParameter => impl_idx.is_none(),
        _ => false,
    });
    let entity = if inside_non_entity {
        Entity::Skip
    } else {
        info.and_then(|i| i.kind.enum_value().ok())
            .and_then(entity_from_kind)
            .unwrap_or(by_descriptor)
    };

    let parent_id = if let Some(i) = impl_idx {
        // `impl#[Foo]bar().` belongs to the type `Foo` that sits next to the impl block.
        let mut owner: Vec<Descriptor> = descs[..i].to_vec();
        owner.push(type_descriptor(&descs[i + 1].name));
        Some(id_of(&package, &owner))
    } else if n >= 2 {
        Some(id_of(&package, &descs[..n - 1]))
    } else {
        None
    };

    let mut names: Vec<String> = Vec::new();
    let mut i = 0;
    while i < n {
        if Some(i) == impl_idx {
            names.push(descs[i + 1].name.clone());
            i += 1;
            while i + 1 < n && matches!(suffix_of(&descs[i + 1]), Suffix::TypeParameter) {
                i += 1;
            }
        } else if matches!(
            suffix_of(&descs[i]),
            Suffix::Type | Suffix::Term | Suffix::Method | Suffix::Macro
        ) {
            names.push(descs[i].name.clone());
        }
        i += 1;
    }

    let name = match info {
        Some(i) if !i.display_name.is_empty() => i.display_name.clone(),
        _ => last.name.clone(),
    };

    Parsed {
        entity,
        id: id_of(&package, descs),
        parent_id,
        package,
        name,
        qualified: names.join("."),
    }
}

fn squash(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn clip(text: String, max: usize) -> String {
    if text.chars().count() <= max {
        return text;
    }
    let mut out: String = text.chars().take(max).collect();
    out.push('…');
    out
}

fn fenced_code(doc: &str) -> Option<String> {
    let rest = doc.trim().strip_prefix("```")?;
    let body = &rest[rest.find('\n')? + 1..];
    Some(body[..body.rfind("```")?].to_string())
}

/// The hover signature, e.g. `def render( self ) -> str:` or `(method) render() => string`.
pub fn signature_of(info: &SymbolInformation) -> Option<String> {
    let sig = squash(&info.signature_documentation.text);
    if !sig.is_empty() {
        return Some(clip(sig, 300));
    }
    info.documentation
        .iter()
        .find_map(|d| fenced_code(d))
        .map(|code| clip(squash(&code), 300))
        .filter(|s| !s.is_empty())
}

/// Prose documentation (doc comments, docstrings), without the signature block.
pub fn doc_of(info: &SymbolInformation) -> Option<String> {
    let parts: Vec<&str> = info
        .documentation
        .iter()
        .map(|d| d.trim())
        .filter(|d| !d.is_empty() && !d.starts_with("```") && !d.starts_with("(module)"))
        .collect();
    if parts.is_empty() {
        None
    } else {
        Some(clip(parts.join("\n"), 500))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protobuf::EnumOrUnknown;

    // Symbols below are copied from what the real indexers emitted for the fixtures.
    const TS: &str = "scip-typescript npm fixture-ts 1.0.0 ";
    const PY: &str = "scip-python python fixture-py 0.1.0 ";
    const RS: &str = "rust-analyzer cargo codebase-context-graph 0.1.0 ";

    fn ts(descriptors: &str) -> Parsed {
        analyze(&format!("{TS}{descriptors}"), None)
    }

    #[test]
    fn typescript_kinds_come_from_descriptors_because_the_indexer_leaves_kind_empty() {
        let method = ts("src/`shapes.ts`/Chart#render().");
        assert_eq!(method.entity, Entity::Symbol("METHOD"));
        assert_eq!(method.name, "render");
        assert_eq!(method.qualified, "Chart.render");
        assert_eq!(method.id, "npm:fixture-ts:src/`shapes.ts`/Chart#render().");
        assert_eq!(
            method.parent_id.as_deref(),
            Some("npm:fixture-ts:src/`shapes.ts`/Chart#")
        );

        assert_eq!(ts("src/`shapes.ts`/exportedFn().").entity, Entity::Symbol("FUNCTION"));
        assert_eq!(ts("src/`shapes.ts`/arrowFn.").entity, Entity::Symbol("VARIABLE"));
        assert_eq!(ts("src/`shapes.ts`/Chart#title.").entity, Entity::Symbol("FIELD"));
        assert_eq!(ts("src/`shapes.ts`/Chart#").entity, Entity::Symbol("TYPE"));
    }

    #[test]
    fn parameters_locals_and_unparsable_symbols_are_not_entities() {
        assert_eq!(ts("src/`shapes.ts`/exportedFn().(a)").entity, Entity::Skip);
        assert_eq!(analyze("local 12", None).entity, Entity::Skip);
        assert_eq!(analyze("", None).entity, Entity::Skip);
        assert_eq!(analyze("not a symbol", None).entity, Entity::Skip);
    }

    #[test]
    fn file_level_modules_are_recognised_for_each_indexer() {
        assert_eq!(ts("src/`shapes.ts`/").entity, Entity::Module);
        assert_eq!(analyze(&format!("{PY}`pkg.shapes`/__init__:"), None).entity, Entity::Module);
        assert_eq!(analyze(&format!("{RS}config/"), None).entity, Entity::Module);
    }

    #[test]
    fn things_declared_inside_a_parameter_are_not_entities() {
        // The `name` property of `function Greeting(props: { name: string })`.
        assert_eq!(ts("src/`view.tsx`/Greeting().(props)typeLiteral0:name.").entity, Entity::Skip);
        // ...but a real member of a type is, and so is a method in rust-analyzer's impl encoding.
        assert_eq!(ts("src/`view.tsx`/Props#name.").entity, Entity::Symbol("FIELD"));
        assert_eq!(
            analyze(&format!("{RS}impl#[GuiApp]run_command()."), None).entity,
            Entity::Symbol("METHOD")
        );
    }

    #[test]
    fn indexer_bookkeeping_symbols_are_not_entities() {
        // What scip-typescript emits for `module.exports = { plainFn, Helper }`.
        assert_eq!(ts("src/`util.js`/plainFn0:").entity, Entity::Skip);
        assert_eq!(ts("src/`util.js`/Helper0:").entity, Entity::Skip);
    }

    #[test]
    fn python_symbols() {
        let method = analyze(&format!("{PY}`pkg.shapes`/Chart#render()."), None);
        assert_eq!(method.entity, Entity::Symbol("METHOD"));
        assert_eq!(method.id, "python:fixture-py:`pkg.shapes`/Chart#render().");
        let function = analyze(&format!("{PY}`pkg.shapes`/plain()."), None);
        assert_eq!(function.entity, Entity::Symbol("FUNCTION"));
    }

    #[test]
    fn rust_impl_methods_are_attached_to_their_type() {
        let inherent = analyze(&format!("{RS}impl#[GuiApp]run_command()."), None);
        assert_eq!(inherent.entity, Entity::Symbol("METHOD"));
        assert_eq!(inherent.name, "run_command");
        assert_eq!(inherent.qualified, "GuiApp.run_command");
        assert_eq!(inherent.parent_id.as_deref(), Some("cargo:codebase-context-graph:GuiApp#"));

        let trait_impl = analyze(&format!("{RS}impl#[GuiApp][App]update()."), None);
        assert_eq!(trait_impl.qualified, "GuiApp.update");
        assert_eq!(trait_impl.parent_id.as_deref(), Some("cargo:codebase-context-graph:GuiApp#"));

        let in_module = analyze(&format!("{RS}parser/impl#[Graph]len()."), None);
        assert_eq!(
            in_module.parent_id.as_deref(),
            Some("cargo:codebase-context-graph:parser/Graph#")
        );
    }

    #[test]
    fn indexer_provided_kind_wins_over_the_descriptor_guess() {
        let mut info = SymbolInformation::default();
        info.kind = EnumOrUnknown::new(Kind::Struct);
        info.display_name = "Config".to_string();
        let parsed = analyze(&format!("{RS}config/Config#"), Some(&info));
        assert_eq!(parsed.entity, Entity::Symbol("STRUCT"));
        assert_eq!(parsed.name, "Config");

        info.kind = EnumOrUnknown::new(Kind::StaticMethod);
        let parsed = analyze(&format!("{RS}impl#[Config]new()."), Some(&info));
        assert_eq!(parsed.entity, Entity::Symbol("METHOD"));

        info.kind = EnumOrUnknown::new(Kind::Parameter);
        assert_eq!(analyze(&format!("{RS}f().(x)"), Some(&info)).entity, Entity::Skip);
    }

    #[test]
    fn package_identifies_external_dependencies() {
        let std_path =
            analyze("rust-analyzer cargo std https://github.com/rust-lang/rust/library/std path/Path#", None);
        assert_eq!(std_path.package, Some(("cargo".into(), "std".into())));
        let builtin = analyze("scip-python python python-stdlib 3.11 builtins/str#", None);
        assert_eq!(builtin.package, Some(("python".into(), "python-stdlib".into())));
    }

    #[test]
    fn ids_do_not_depend_on_the_package_version() {
        let a = analyze("scip-python python fixture-py 0.1.0 `pkg.a`/f().", None);
        let b = analyze("scip-python python fixture-py 9f3c2e1 `pkg.a`/f().", None);
        assert_eq!(a.id, b.id);
    }

    #[test]
    fn signatures_come_from_signature_documentation_or_a_fenced_hover_block() {
        let mut info = SymbolInformation::default();
        info.documentation = vec!["```python\ndef render(\n  self\n) -> str:\n```".to_string()];
        assert_eq!(signature_of(&info).as_deref(), Some("def render( self ) -> str:"));

        info.documentation =
            vec!["```ts\n(method) render() => string\n```".to_string(), "Draws it.".to_string()];
        assert_eq!(signature_of(&info).as_deref(), Some("(method) render() => string"));
        assert_eq!(doc_of(&info).as_deref(), Some("Draws it."));

        let mut rust = SymbolInformation::default();
        rust.signature_documentation.mut_or_insert_default().text = "enum OutputLine".to_string();
        assert_eq!(signature_of(&rust).as_deref(), Some("enum OutputLine"));
        assert_eq!(doc_of(&rust), None);

        assert_eq!(signature_of(&SymbolInformation::default()), None);
    }
}
