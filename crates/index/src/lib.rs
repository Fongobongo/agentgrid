//! Symbol extraction for repo indexing: "where is `foo` defined" answered by
//! parsing instead of by grep.
//!
//! The line-prefix heuristic this replaces could only see column-0 signatures,
//! which for Rust means *nothing inside an `impl` block* — so the most common
//! question an agent asks about a codebase had no answer. Parsing fixes that
//! class of miss by construction: a symbol is a node in the grammar, wherever
//! it is nested.
//!
//! Two deliberate limits:
//!
//! - **Definitions only.** References are not recorded: an index of every
//!   identifier is a grep with extra steps, and the question is where to look.
//! - **Nesting is one level of meaning.** A method reports the type it belongs
//!   to (`container`), not the full path through nested modules — the container
//!   is what disambiguates two `new`s, and deeper paths mostly add noise.
//!
//! A file that fails to parse contributes no symbols rather than an error: a
//! broken tree is the normal state of a repository an agent is mid-edit on, and
//! indexing is a convenience the attempt must never fail on.

use std::path::Path;

mod extract;

pub use extract::symbols;

/// Languages the indexer recognises, by file extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Lang {
    Rust,
    TypeScript,
    Tsx,
    JavaScript,
    Jsx,
    Python,
    Go,
    Java,
    C,
    Cpp,
}

impl Lang {
    /// Language for a path, by extension (case-insensitive). `None` for files
    /// the indexer has nothing to say about.
    pub fn of_path(path: &Path) -> Option<Lang> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        Some(match ext.as_str() {
            "rs" => Lang::Rust,
            "ts" | "mts" | "cts" => Lang::TypeScript,
            "tsx" => Lang::Tsx,
            "js" | "mjs" | "cjs" => Lang::JavaScript,
            "jsx" => Lang::Jsx,
            "py" | "pyi" => Lang::Python,
            "go" => Lang::Go,
            // No Kotlin grammar is compiled in, and feeding `.kt` to the Java
            // grammar would report symbols from a mis-parse — worse than the
            // honest "not indexed".
            "java" => Lang::Java,
            "c" | "h" => Lang::C,
            "cpp" | "cc" | "cxx" | "hpp" | "hh" => Lang::Cpp,
            _ => return None,
        })
    }

    /// Stable tag used in the index packet and in summaries.
    pub fn tag(self) -> &'static str {
        match self {
            Lang::Rust => "rust",
            Lang::TypeScript => "typescript",
            Lang::Tsx => "tsx",
            Lang::JavaScript => "javascript",
            Lang::Jsx => "jsx",
            Lang::Python => "python",
            Lang::Go => "go",
            Lang::Java => "java",
            Lang::C => "c",
            Lang::Cpp => "cpp",
        }
    }

    /// True when a real grammar is compiled in for this language.
    pub fn has_grammar(self) -> bool {
        extract::grammar(self).is_some()
    }
}

/// What a definition is, coarse enough to stay comparable across languages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolKind {
    Function,
    /// A function attached to a type: Rust `impl` fn, TS/Python class method,
    /// Go method with a receiver.
    Method,
    Struct,
    Enum,
    Trait,
    Class,
    Interface,
    TypeAlias,
    Module,
    Constant,
    Macro,
}

impl SymbolKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SymbolKind::Function => "function",
            SymbolKind::Method => "method",
            SymbolKind::Struct => "struct",
            SymbolKind::Enum => "enum",
            SymbolKind::Trait => "trait",
            SymbolKind::Class => "class",
            SymbolKind::Interface => "interface",
            SymbolKind::TypeAlias => "type",
            SymbolKind::Module => "module",
            SymbolKind::Constant => "const",
            SymbolKind::Macro => "macro",
        }
    }
}

/// One definition, positioned so a reader (or an agent) can jump to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Symbol {
    pub name: String,
    pub kind: SymbolKind,
    /// 1-based line of the definition's start.
    pub line: usize,
    /// Enclosing type/module name, e.g. `SandboxKind` for an `impl` method.
    pub container: Option<String>,
}

impl Symbol {
    /// `Container::name` when nested, bare `name` otherwise.
    pub fn qualified_name(&self) -> String {
        match &self.container {
            Some(c) => format!("{c}::{}", self.name),
            None => self.name.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(lang: Lang, src: &str) -> Vec<String> {
        symbols(lang, src).into_iter().map(|s| s.name).collect()
    }

    fn find<'a>(syms: &'a [Symbol], name: &str) -> &'a Symbol {
        syms.iter()
            .find(|s| s.name == name)
            .unwrap_or_else(|| panic!("{name} missing from {syms:?}"))
    }

    #[test]
    fn extensions_map_to_languages() {
        assert_eq!(Lang::of_path(Path::new("a/b.rs")), Some(Lang::Rust));
        assert_eq!(Lang::of_path(Path::new("x.tsx")), Some(Lang::Tsx));
        assert_eq!(Lang::of_path(Path::new("X.PY")), Some(Lang::Python));
        assert_eq!(Lang::of_path(Path::new("a.hpp")), Some(Lang::Cpp));
        assert_eq!(Lang::of_path(Path::new("README.md")), None);
        assert_eq!(Lang::of_path(Path::new("Makefile")), None);
        // No Kotlin grammar: silently parsing it as Java would invent symbols.
        assert_eq!(Lang::of_path(Path::new("Main.kt")), None);
    }

    #[test]
    fn rust_methods_inside_impl_are_found() {
        // The whole point: the old scanner only saw column-0 lines, so every
        // method in an `impl` block was invisible.
        let src = r#"
pub struct Server { port: u16 }

impl Server {
    pub fn new(port: u16) -> Self { Self { port } }
    fn handle(&self) {}
}

impl std::fmt::Display for Server {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result { Ok(()) }
}
"#;
        let syms = symbols(Lang::Rust, src);
        let new = find(&syms, "new");
        assert_eq!(new.kind, SymbolKind::Method);
        assert_eq!(new.container.as_deref(), Some("Server"));
        assert_eq!(new.line, 5);
        assert_eq!(find(&syms, "handle").container.as_deref(), Some("Server"));
        // A trait impl keys off the type, not the trait: that is what a caller
        // searches by.
        assert_eq!(find(&syms, "fmt").container.as_deref(), Some("Server"));
        let server = find(&syms, "Server");
        assert_eq!(server.kind, SymbolKind::Struct);
        assert_eq!(server.container, None);
    }

    #[test]
    fn rust_top_level_items_keep_their_kinds() {
        let src = "pub fn alpha() {}\ntrait Greet {}\nenum Color { Red }\ntype Alias = u8;\nmod inner { pub fn nested() {} }\nmacro_rules! mac { () => {} }\nconst LIMIT: u8 = 1;\n";
        let syms = symbols(Lang::Rust, src);
        assert_eq!(find(&syms, "alpha").kind, SymbolKind::Function);
        assert_eq!(find(&syms, "Greet").kind, SymbolKind::Trait);
        assert_eq!(find(&syms, "Color").kind, SymbolKind::Enum);
        assert_eq!(find(&syms, "Alias").kind, SymbolKind::TypeAlias);
        assert_eq!(find(&syms, "mac").kind, SymbolKind::Macro);
        assert_eq!(find(&syms, "LIMIT").kind, SymbolKind::Constant);
        // A nested module is a container, not a flattened namespace.
        assert_eq!(find(&syms, "inner").kind, SymbolKind::Module);
        assert_eq!(find(&syms, "nested").container.as_deref(), Some("inner"));
    }

    #[test]
    fn typescript_classes_and_their_methods() {
        let src = r#"
export class App {
  render(): void {}
  private helper() {}
}
export interface Props { x: number }
export function boot() {}
"#;
        let syms = symbols(Lang::TypeScript, src);
        assert_eq!(find(&syms, "App").kind, SymbolKind::Class);
        let render = find(&syms, "render");
        assert_eq!(render.kind, SymbolKind::Method);
        assert_eq!(render.container.as_deref(), Some("App"));
        assert_eq!(find(&syms, "Props").kind, SymbolKind::Interface);
        assert_eq!(find(&syms, "boot").kind, SymbolKind::Function);
    }

    #[test]
    fn python_nested_defs_and_async() {
        let src = "class Store:\n    def get(self):\n        pass\n\nasync def run():\n    pass\n";
        let syms = symbols(Lang::Python, src);
        let get = find(&syms, "get");
        assert_eq!(get.kind, SymbolKind::Method);
        assert_eq!(get.container.as_deref(), Some("Store"));
        assert_eq!(find(&syms, "run").kind, SymbolKind::Function);
    }

    #[test]
    fn go_methods_report_their_receiver_type() {
        let src = "package main\n\nfunc main() {}\nfunc (s *Server) Start() {}\ntype Server struct{}\ntype Handler interface{}\n";
        let syms = symbols(Lang::Go, src);
        assert_eq!(find(&syms, "main").kind, SymbolKind::Function);
        let start = find(&syms, "Start");
        assert_eq!(start.kind, SymbolKind::Method);
        assert_eq!(start.container.as_deref(), Some("Server"));
        assert_eq!(find(&syms, "Server").kind, SymbolKind::Struct);
        assert_eq!(find(&syms, "Handler").kind, SymbolKind::Interface);
    }

    #[test]
    fn java_classes_and_methods() {
        let src = "class Foo {\n  void bar() {}\n}\ninterface Baz {}\n";
        let syms = symbols(Lang::Java, src);
        assert_eq!(find(&syms, "Foo").kind, SymbolKind::Class);
        assert_eq!(find(&syms, "bar").container.as_deref(), Some("Foo"));
        assert_eq!(find(&syms, "Baz").kind, SymbolKind::Interface);
    }

    #[test]
    fn c_functions_and_structs() {
        let src = "struct point { int x; };\nint add(int a, int b) { return a + b; }\n";
        let syms = symbols(Lang::C, src);
        assert_eq!(find(&syms, "add").kind, SymbolKind::Function);
        assert_eq!(find(&syms, "point").kind, SymbolKind::Struct);
    }

    #[test]
    fn a_broken_document_yields_what_parsed_instead_of_failing() {
        // Half a file still indexes the half that parses: an agent working on
        // broken code must not lose the map of everything else.
        let syms = symbols(Lang::Rust, "fn good() {}\nfn broken( {");
        assert!(names(Lang::Rust, "fn good() {}\nfn broken( {").contains(&"good".to_string()));
        let _ = syms;
    }

    #[test]
    fn empty_source_has_no_symbols() {
        for lang in [
            Lang::Rust,
            Lang::TypeScript,
            Lang::JavaScript,
            Lang::Python,
            Lang::Go,
            Lang::Java,
            Lang::C,
        ] {
            assert!(symbols(lang, "").is_empty(), "{:?}", lang);
        }
    }

    #[test]
    fn every_language_has_a_grammar() {
        for lang in [
            Lang::Rust,
            Lang::TypeScript,
            Lang::Tsx,
            Lang::JavaScript,
            Lang::Jsx,
            Lang::Python,
            Lang::Go,
            Lang::Java,
            Lang::C,
            Lang::Cpp,
        ] {
            assert!(lang.has_grammar(), "{lang:?} has no grammar");
        }
    }
}
