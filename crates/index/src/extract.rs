//! The parse half: one tree walk, one definition table per language.

use tree_sitter::{Language, Node, Parser};

use crate::{Lang, Symbol, SymbolKind};

/// Grammar compiled in for `lang`, if any.
pub(crate) fn grammar(lang: Lang) -> Option<Language> {
    Some(match lang {
        Lang::Rust => tree_sitter_rust::LANGUAGE.into(),
        Lang::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        Lang::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
        Lang::JavaScript | Lang::Jsx => tree_sitter_javascript::LANGUAGE.into(),
        Lang::Python => tree_sitter_python::LANGUAGE.into(),
        Lang::Go => tree_sitter_go::LANGUAGE.into(),
        Lang::Java => tree_sitter_java::LANGUAGE.into(),
        Lang::C => tree_sitter_c::LANGUAGE.into(),
        Lang::Cpp => tree_sitter_cpp::LANGUAGE.into(),
    })
}

/// Every definition in `source`, in document order.
///
/// A file that fails to parse contributes no symbols rather than an error: a
/// broken tree is a normal state for a repository an agent is mid-edit on, and
/// indexing is a convenience the attempt must never fail on.
pub fn symbols(lang: Lang, source: &str) -> Vec<Symbol> {
    if source.trim().is_empty() {
        return Vec::new();
    }
    let Some(grammar) = grammar(lang) else {
        return Vec::new();
    };
    let mut parser = Parser::new();
    if parser.set_language(&grammar).is_err() {
        return Vec::new();
    }
    let Some(tree) = parser.parse(source, None) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    walk(tree.root_node(), source, lang, None, &mut out);
    out
}

fn walk(node: Node, src: &str, lang: Lang, container: Option<&str>, out: &mut Vec<Symbol>) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        // Every definition has at least a name child, so a leaf is neither a
        // definition nor worth descending into. On real files tokens outnumber
        // items by an order of magnitude, and this check is one C call.
        if child.child_count() == 0 {
            continue;
        }
        // A node can name its own scope without being a definition
        // (`impl Server`), and a Go method scopes itself to its receiver.
        // `container_of` returns `None` for almost every node, so the common
        // path must not allocate — the walk visits every node in the file.
        let own_scope = container_of(lang, child, src);
        let scope = own_scope.as_deref().or(container);
        if let Some((kind, name)) = definition(lang, child, src) {
            // A function inside a type is a method: the distinction is what
            // tells a reader whether `new` is callable on its own.
            let kind = match (kind, scope) {
                (SymbolKind::Function, Some(_)) => SymbolKind::Method,
                (k, _) => k,
            };
            out.push(Symbol {
                name: name.clone(),
                kind,
                line: child.start_position().row + 1,
                container: scope.map(str::to_string),
            });
            walk(child, src, lang, Some(&name), out);
            continue;
        }
        walk(child, src, lang, scope, out);
    }
}

/// `(kind, name)` when this node is a definition.
fn definition(lang: Lang, node: Node, src: &str) -> Option<(SymbolKind, String)> {
    use Lang::*;
    use SymbolKind as K;
    let kind = match (lang, node.kind()) {
        (Rust, "function_item") => K::Function,
        (Rust, "struct_item") | (Rust, "union_item") => K::Struct,
        (Rust, "enum_item") => K::Enum,
        (Rust, "trait_item") => K::Trait,
        (Rust, "type_item") => K::TypeAlias,
        (Rust, "mod_item") => K::Module,
        (Rust, "const_item") | (Rust, "static_item") => K::Constant,
        (Rust, "macro_definition") => K::Macro,

        (TypeScript | Tsx | JavaScript | Jsx, "function_declaration")
        | (TypeScript | Tsx | JavaScript | Jsx, "generator_function_declaration")
        | (TypeScript | Tsx | JavaScript | Jsx, "function_signature") => K::Function,
        (TypeScript | Tsx | JavaScript | Jsx, "method_definition") => K::Method,
        (TypeScript | Tsx | JavaScript | Jsx, "class_declaration")
        | (TypeScript | Tsx, "abstract_class_declaration") => K::Class,
        (TypeScript | Tsx, "interface_declaration") => K::Interface,
        (TypeScript | Tsx, "type_alias_declaration") => K::TypeAlias,
        (TypeScript | Tsx, "enum_declaration") => K::Enum,

        (Python, "function_definition") => K::Function,
        (Python, "class_definition") => K::Class,

        (Go, "function_declaration") => K::Function,
        (Go, "method_declaration") => K::Method,
        (Go, "type_spec") => match node.child_by_field_name("type").map(|t| t.kind()) {
            Some("struct_type") => K::Struct,
            Some("interface_type") => K::Interface,
            _ => K::TypeAlias,
        },

        (Java, "class_declaration") | (Java, "record_declaration") => K::Class,
        (Java, "interface_declaration") | (Java, "annotation_type_declaration") => K::Interface,
        (Java, "enum_declaration") => K::Enum,
        (Java, "method_declaration") | (Java, "constructor_declaration") => K::Method,

        (C, "function_definition") | (Cpp, "function_definition") => K::Function,
        (C, "struct_specifier") | (Cpp, "struct_specifier") => K::Struct,
        (C, "enum_specifier") | (Cpp, "enum_specifier") => K::Enum,
        (C, "type_definition") | (Cpp, "type_definition") => K::TypeAlias,
        (Cpp, "class_specifier") => K::Class,
        (Cpp, "namespace_definition") => K::Module,

        _ => return None,
    };
    // `struct point;` (a forward declaration) and an anonymous `struct { … }`
    // are not definitions a reader can jump to: require a body and a name.
    if matches!(
        node.kind(),
        "struct_specifier" | "class_specifier" | "enum_specifier"
    ) && node.child_by_field_name("body").is_none()
    {
        return None;
    }
    let name = name_of(lang, node, src)?;
    Some((kind, name))
}

/// Name of a definition.
///
/// Most grammars put it in a `name` field. C and C++ do not: the name hides
/// inside the declarator chain (`int add(int a, int b)` →
/// `function_declarator` → `identifier`), which is also where a pointer or a
/// parenthesised declarator can add a level.
fn name_of(lang: Lang, node: Node, src: &str) -> Option<String> {
    if let Some(name_node) = node.child_by_field_name("name") {
        return text_name(name_node, src);
    }
    match (lang, node.kind()) {
        (Lang::C | Lang::Cpp, "function_definition") | (Lang::C | Lang::Cpp, "type_definition") => {
            declarator_name(node, src)
        }
        _ => None,
    }
}

/// Follow `declarator` links until an identifier is reached.
fn declarator_name(node: Node, src: &str) -> Option<String> {
    let mut cur = node.child_by_field_name("declarator")?;
    // Bounded: a declarator chain is a handful of links, but a pathological
    // file should not be able to spin here.
    for _ in 0..16 {
        if matches!(
            cur.kind(),
            "identifier" | "field_identifier" | "type_identifier" | "operator_name"
        ) {
            return text_name(cur, src);
        }
        cur = cur.child_by_field_name("declarator")?;
    }
    None
}

fn text_name(name_node: Node, src: &str) -> Option<String> {
    let raw = name_node.utf8_text(src.as_bytes()).ok()?.trim();
    // String keys (`{"a-b": …}`) and computed keys (`[k]: …`) are not names a
    // reader can search for; `quote` strips the former, `[` rejects the latter.
    let cleaned = raw
        .trim_matches(|c| c == '"' || c == '\'' || c == '`')
        .trim();
    if cleaned.is_empty() || cleaned.starts_with('[') || cleaned.contains('\n') {
        return None;
    }
    Some(cleaned.to_string())
}

/// Name of the scope a node introduces without being a definition itself.
fn container_of(lang: Lang, node: Node, src: &str) -> Option<String> {
    match (lang, node.kind()) {
        // `impl Server { … }` — the type is the useful part; a trait impl keys
        // off the type too, since that is what a caller searches by.
        (Lang::Rust, "impl_item") => {
            let t = node.child_by_field_name("type")?;
            let text = t.utf8_text(src.as_bytes()).ok()?.trim();
            let head = text.split(['<', ' ', '{']).next().unwrap_or(text);
            (!head.is_empty()).then(|| head.to_string())
        }
        // `func (s *Server) Start()` — the receiver is not a `name` field.
        (Lang::Go, "method_declaration") => {
            let recv = node.child_by_field_name("receiver")?;
            receiver_type(recv.utf8_text(src.as_bytes()).ok()?)
        }
        _ => None,
    }
}

/// `(s *Server)` / `(s Server[T])` → `Server`.
fn receiver_type(text: &str) -> Option<String> {
    let inner = text.trim().trim_start_matches('(').trim_end_matches(')');
    let last = inner.split_whitespace().last()?;
    let ty = last.trim_start_matches('*').trim_start_matches('&');
    let ty = ty.split('[').next().unwrap_or(ty);
    (!ty.is_empty()).then(|| ty.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn receiver_type_strips_the_pointer_and_the_generic() {
        assert_eq!(receiver_type("(s *Server)").as_deref(), Some("Server"));
        assert_eq!(receiver_type("(s Server)").as_deref(), Some("Server"));
        assert_eq!(receiver_type("(s *List[T])").as_deref(), Some("List"));
        assert_eq!(receiver_type("()"), None);
    }

    #[test]
    fn an_anonymous_struct_is_not_a_definition() {
        // `struct { int x; } v;` has no name to jump to.
        let syms = symbols(Lang::C, "int main() { struct { int x; } v; return v.x; }\n");
        assert!(syms.iter().all(|s| s.name != "v"), "{syms:?}");
        assert!(syms.iter().any(|s| s.name == "main"));
    }

    #[test]
    fn a_forward_declaration_is_not_a_definition() {
        let syms = symbols(Lang::C, "struct point;\nstruct point { int x; };\n");
        assert_eq!(
            syms.iter().filter(|s| s.name == "point").count(),
            1,
            "{syms:?}"
        );
    }
}
