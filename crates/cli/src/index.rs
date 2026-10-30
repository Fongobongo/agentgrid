//! Plan 1.13: `ag index` — offline symbol extraction so an agent without
//! built-in codebase awareness can see what a repo contains before its first
//! attempt. Output is a commit-scoped JSON packet.
//!
//! Symbols come from `agentgrid-index` (tree-sitter), not from line prefixes:
//! the heuristic this replaced only saw column-0 signatures, so every method
//! inside a Rust `impl` block — the most common answer to "where is this
//! defined" — was missing. Imports stay line-based; they are already a prefix
//! grammar and a parser buys nothing there.

use agentgrid_index::{symbols, Lang};
use anyhow::{Context, Result};
use clap::Args;
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Args)]
pub struct IndexArgs {
    /// Repo root to index (default: current dir).
    #[arg(default_value = ".")]
    path: PathBuf,
    /// Write the JSON packet to this path instead of stdout. Useful as a
    /// cache/disk writer (plan 1.13 follow-up) for node-daemon digest inject.
    #[arg(long)]
    out: Option<PathBuf>,
    /// Where is this symbol defined? Prints matches instead of the packet and
    /// exits 1 when nothing matches (grep semantics, so it can gate a script).
    #[arg(long, value_name = "NAME")]
    find: Option<String>,
    /// Cap the number of `--find` matches.
    #[arg(long, default_value_t = 20)]
    limit: usize,
}

#[derive(Debug, Serialize)]
pub struct IndexPacket {
    pub commit: String,
    pub vcs: &'static str,
    pub root: String,
    pub files: Vec<FileIndex>,
    pub summary: IndexSummary,
}

#[derive(Debug, Serialize)]
pub struct FileIndex {
    pub path: String,
    pub lang: &'static str,
    pub symbols: Vec<Sym>,
    pub imports: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct Sym {
    pub kind: &'static str, // see `SymbolKind::as_str`
    pub name: String,
    pub line: usize,
    /// Enclosing type/module, when the definition is nested.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct IndexSummary {
    pub total_files: usize,
    pub total_symbols: usize,
    pub total_imports: usize,
    pub langs: Vec<(String, usize)>, // lang -> file count, sorted desc
}

/// Walk the repo, return a commit-scoped context packet.
pub fn index_repo(root: &Path) -> Result<IndexPacket> {
    let canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let commit = head_sha(&canonical);
    let vcs = if commit.is_empty() { "none" } else { "git" };
    let mut files = Vec::new();
    walk(&canonical, &canonical, &mut files)?;
    files.sort_by(|a, b| a.path.cmp(&b.path));
    let total_symbols = files.iter().map(|f| f.symbols.len()).sum();
    let total_imports = files.iter().map(|f| f.imports.len()).sum();
    let mut langs: Vec<(&'static str, usize)> = Vec::new();
    for f in &files {
        if let Some(slot) = langs.iter_mut().find(|(l, _)| *l == f.lang) {
            slot.1 += 1;
        } else {
            langs.push((f.lang, 1));
        }
    }
    langs.sort_by_key(|(_, n): &(&'static str, usize)| std::cmp::Reverse(*n));
    let total_files = files.len();
    Ok(IndexPacket {
        commit,
        vcs,
        root: canonical.display().to_string(),
        files,
        summary: IndexSummary {
            total_files,
            total_symbols,
            total_imports,
            langs: langs.into_iter().map(|(l, n)| (l.to_string(), n)).collect(),
        },
    })
}

/// Skip list — VCS/build dirs/materialized deps that bloat the index without
/// telling the agent anything about the human-written code.
fn is_pruned(name: &str) -> bool {
    matches!(
        name,
        ".git"
            | ".hg"
            | ".svn"
            | "target"
            | "node_modules"
            | "dist"
            | "build"
            | ".next"
            | "__pycache__"
            | ".venv"
            | "venv"
            | ".idea"
            | ".vscode"
            | ".idx"
    )
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<FileIndex>) -> Result<()> {
    for entry in fs::read_dir(dir).with_context(|| format!("read_dir {}", dir.display()))? {
        let entry = entry?;
        let ft = entry.file_type()?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if ft.is_dir() {
            if is_pruned(&name) {
                continue;
            }
            walk(root, &entry.path(), out)?;
        } else if ft.is_file() {
            let path = entry.path();
            if let Some(lang) = Lang::of_path(&path) {
                if let Ok(src) = fs::read_to_string(&path) {
                    let (symbols, imports) = scan(lang, &src);
                    let rel = path
                        .strip_prefix(root)
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|_| path.display().to_string());
                    if !symbols.is_empty() || !imports.is_empty() {
                        out.push(FileIndex {
                            path: rel,
                            lang: lang.tag(),
                            symbols,
                            imports,
                        });
                    }
                }
            }
        }
    }
    Ok(())
}

/// Symbols from the parser, imports from the line prefix.
fn scan(lang: Lang, src: &str) -> (Vec<Sym>, Vec<String>) {
    let syms = symbols(lang, src)
        .into_iter()
        .map(|s| Sym {
            kind: s.kind.as_str(),
            name: s.name,
            line: s.line,
            container: s.container,
        })
        .collect();
    (syms, imports(lang, src))
}

/// Import/include statements. Still line-prefixed on purpose: an import is a
/// fixed leading keyword, so a parser would add a dependency-sized cost to
/// every file for a result a `strip_prefix` already gives exactly.
fn imports(lang: Lang, src: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in src.lines() {
        let l = line.trim_start();
        match lang {
            Lang::Rust => {
                if let Some(rest) = l.strip_prefix("use ") {
                    let path = rest.split(';').next().unwrap_or(rest).trim();
                    if !path.is_empty() {
                        out.push(path.to_string());
                    }
                }
            }
            Lang::TypeScript | Lang::Tsx | Lang::JavaScript | Lang::Jsx => {
                if !l.starts_with("import ") && !l.starts_with("import{") {
                    continue;
                }
                // `import x from "y"` and bare `import "./y"` both name a
                // source; `import * as ns from "y"` is the former.
                let rest = if let Some(k) = l.find("from ") {
                    &l[k + 5..]
                } else {
                    l.trim_start_matches("import").trim_start()
                };
                let src = rest
                    .trim_start()
                    .trim_start_matches(['"', '\''])
                    .split(['"', '\''])
                    .next()
                    .unwrap_or("")
                    .trim();
                if !src.is_empty() {
                    out.push(src.to_string());
                }
            }
            Lang::Python => {
                if let Some(rest) = l.strip_prefix("from ") {
                    let module = rest.split_whitespace().next().unwrap_or("");
                    if !module.is_empty() {
                        out.push(module.to_string());
                    }
                } else if let Some(rest) = l.strip_prefix("import ") {
                    for n in rest.split(',') {
                        let n = n.split_whitespace().next().unwrap_or("");
                        if !n.is_empty() {
                            out.push(n.to_string());
                        }
                    }
                }
            }
            Lang::Go => {
                // `import "x"` plus the quoted lines of a grouped block.
                let candidate = if let Some(rest) = l.strip_prefix("import ") {
                    Some(rest)
                } else if l.starts_with('"') {
                    Some(l)
                } else {
                    None
                };
                if let Some(rest) = candidate {
                    let src = rest
                        .trim()
                        .trim_start_matches('"')
                        .split('"')
                        .next()
                        .unwrap_or("");
                    if !src.is_empty() {
                        out.push(src.to_string());
                    }
                }
            }
            Lang::Java => {
                if let Some(rest) = l.strip_prefix("import ") {
                    let path = rest
                        .trim_start_matches("static ")
                        .split(';')
                        .next()
                        .unwrap_or(rest)
                        .trim();
                    if !path.is_empty() {
                        out.push(path.to_string());
                    }
                }
            }
            Lang::C | Lang::Cpp => {
                if let Some(rest) = l
                    .strip_prefix("#include ")
                    .or_else(|| l.strip_prefix("#import "))
                {
                    let src = rest
                        .trim()
                        .trim_start_matches(['<', '"'])
                        .split(['>', '"'])
                        .next()
                        .unwrap_or("")
                        .trim();
                    if !src.is_empty() {
                        out.push(src.to_string());
                    }
                }
            }
        }
    }
    out
}

/// A `--find` match and why it ranked where it did.
#[derive(Debug, Serialize)]
struct Hit<'a> {
    path: &'a str,
    line: usize,
    kind: &'static str,
    name: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    container: Option<&'a str>,
    /// 0 = exact name, 1 = prefix, 2 = substring.
    rank: u8,
}

/// Rank symbols against a query.
///
/// Three tiers, because "where is `new`" and "where is something with `new` in
/// it" are different questions: an exact hit must not be buried under a dozen
/// near-misses. `Container::name` is matched too, so `SandboxKind::from_env`
/// works without knowing that the container is stored separately.
fn find_symbols<'a>(packet: &'a IndexPacket, query: &str, limit: usize) -> Vec<Hit<'a>> {
    let q = query.trim();
    let lq = q.to_lowercase();
    let mut hits = Vec::new();
    for file in &packet.files {
        for sym in &file.symbols {
            let qualified = match &sym.container {
                Some(c) => format!("{c}::{}", sym.name),
                None => sym.name.clone(),
            };
            let rank = if qualified == q || sym.name == q {
                0
            } else if qualified.to_lowercase().starts_with(&lq) {
                1
            } else if qualified.to_lowercase().contains(&lq) {
                2
            } else {
                continue;
            };
            hits.push(Hit {
                path: &file.path,
                line: sym.line,
                kind: sym.kind,
                name: &sym.name,
                container: sym.container.as_deref(),
                rank,
            });
        }
    }
    // Ties break by path then line so the output is stable across runs (an
    // unstable answer makes a script that consumes it flaky).
    hits.sort_by(|a, b| (a.rank, a.path, a.line, &a.name).cmp(&(b.rank, b.path, b.line, &b.name)));
    hits.truncate(limit);
    hits
}

/// Read HEAD sha via `git rev-parse HEAD` at root. Empty on any failure (no
/// git, no repo). Commits-stamping requires non-empty; we degrade to vcs=none.
fn head_sha(root: &Path) -> String {
    std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

pub fn cmd_index(args: IndexArgs, json: bool) -> Result<()> {
    let packet = index_repo(&args.path)?;
    if let Some(query) = &args.find {
        let hits = find_symbols(&packet, query, args.limit);
        if hits.is_empty() {
            anyhow::bail!("no symbol matching {query:?} under {}", packet.root);
        }
        if json {
            println!("{}", serde_json::to_string_pretty(&hits)?);
            return Ok(());
        }
        for h in &hits {
            let qualified = match h.container {
                Some(c) => format!("{c}::{}", h.name),
                None => h.name.to_string(),
            };
            println!("{}:{}  {:<9} {}", h.path, h.line, h.kind, qualified);
        }
        if hits.len() == args.limit {
            println!(
                "\n(limit {} reached — narrow the query or raise --limit)",
                args.limit
            );
        }
        return Ok(());
    }
    // Plan 1.13 follow-up: `--out <path>` writes the JSON packet to a file
    // (cache/disk writer for the node-daemon digest-injector), bypassing the
    // stdout pretty-printer.
    if let Some(out) = &args.out {
        let s = serde_json::to_string_pretty(&packet)?;
        std::fs::write(out, s)?;
        println!(
            "wrote {} bytes to {}",
            std::fs::metadata(out)?.len(),
            out.display()
        );
        return Ok(());
    }
    if json {
        let s = serde_json::to_string_pretty(&packet)?;
        println!("{s}");
        return Ok(());
    }
    println!("# agentgrid knowledge index: {}", packet.root);
    println!("commit: {} ({})\n", packet.commit, packet.vcs);
    println!(
        "{} files · {} symbols · {} imports",
        packet.files.len(),
        packet.summary.total_symbols,
        packet.summary.total_imports
    );
    let langs: Vec<String> = packet
        .summary
        .langs
        .iter()
        .map(|(l, n)| format!("{l}({n})"))
        .collect();
    if !langs.is_empty() {
        println!("langs: {}\n", langs.join(", "));
    }
    for f in &packet.files {
        println!("# {} [{}]", f.path, f.lang);
        if !f.symbols.is_empty() {
            let names: Vec<String> = f
                .symbols
                .iter()
                .map(|s| format!("{} {}", s.kind, s.name))
                .collect();
            println!("  {}", names.join(" · "));
        }
        if !f.imports.is_empty() {
            println!("  imports: {}", f.imports.join(" · "));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_repo() -> tempfile::TempDir {
        tempfile::tempdir().expect("tmpdir")
    }

    #[test]
    fn index_small_rust_repo_yields_top_level_symbols_and_imports() {
        let dir = tmp_repo();
        let root = dir.path();
        std::fs::write(
            root.join("lib.rs"),
            "use serde::Serialize;\nuse std::path::Path;\n\npub fn alpha(x: u32) -> u32 { x + 1 }\nfn beta() {}\n\npub struct Point { x: u32 }\nenum Color { Red }\npub trait Greet { fn hello(&self); }\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("src").join("inner.rs"),
            "fn helper() {}     // top-level\nstruct Inner { a: i32 }\n",
        )
        .unwrap();
        // Won't index (.md):
        std::fs::write(root.join("README.md"), "# ignore me\n").unwrap();

        let packet = index_repo(root).expect("index");
        assert_eq!(packet.vcs, "none"); // not a git repo, head_sha empty
        assert!(packet.commit.is_empty());

        let lib = packet
            .files
            .iter()
            .find(|f| f.path.ends_with("lib.rs"))
            .expect("lib.rs");
        let names: Vec<&str> = lib.symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"alpha"), "alpha missing: {names:?}");
        assert!(names.contains(&"beta"), "beta missing: {names:?}");
        assert!(names.contains(&"Point"));
        assert!(names.contains(&"Color"));
        assert!(names.contains(&"Greet"));
        // imports captured
        assert!(
            lib.imports.iter().any(|i| i == "serde::Serialize"),
            "serde import missing"
        );
        assert!(lib.imports.iter().any(|i| i == "std::path::Path"));

        let inner = packet
            .files
            .iter()
            .find(|f| f.path.contains("inner.rs"))
            .expect("inner.rs");
        assert!(inner.symbols.iter().any(|s| s.name == "helper"));
        assert!(inner.symbols.iter().any(|s| s.name == "Inner"));

        // README not indexed.
        assert!(!packet.files.iter().any(|f| f.path.ends_with(".md")));
        // Summary totals populated (plan 1.13 follow-up: used by digest inject).
        assert_eq!(
            packet.summary.total_files, 2,
            "total_files wrong: {}",
            packet.summary.total_files
        );
        assert!(
            packet.summary.total_symbols >= 4,
            "total_symbols wrong: {}",
            packet.summary.total_symbols
        );
        // Rust appears in langs summary.
        assert!(packet
            .summary
            .langs
            .iter()
            .any(|(l, n)| l == "rust" && *n == 2));
    }

    #[test]
    fn index_ts_imports_and_classes() {
        let dir = tmp_repo();
        let root = dir.path();
        std::fs::write(
            root.join("app.tsx"),
            "import React from 'react';\nimport { axum } from \"./server\";\nexport class App {}\nexport function render() {}\nasync function helper() {}\n",
        )
        .unwrap();
        let packet = index_repo(root).expect("index");
        let app = &packet.files[0];
        assert!(app.imports.iter().any(|i| i == "react"));
        assert!(app.imports.iter().any(|i| i == "./server"));
        assert!(app.symbols.iter().any(|s| s.name == "App"));
        assert!(app.symbols.iter().any(|s| s.name == "render"));
        assert!(app.symbols.iter().any(|s| s.name == "helper"));
    }

    #[test]
    fn index_python_defs_and_imports() {
        let dir = tmp_repo();
        let root = dir.path();
        std::fs::write(
            root.join("m.py"),
            "from collections import defaultdict\nimport os, sys\n\nasync def run(): pass\nclass Foo: pass\ndef add(a, b):\n    return a + b\n",
        )
        .unwrap();
        let packet = index_repo(root).expect("index");
        let m = &packet.files[0];
        let names: Vec<&str> = m.symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"run"));
        assert!(names.contains(&"Foo"));
        assert!(names.contains(&"add"));
        assert!(m.imports.iter().any(|i| i == "collections"));
        assert!(m.imports.iter().any(|i| i == "os"));
        assert!(m.imports.iter().any(|i| i == "sys"));
    }

    #[test]
    fn index_skips_build_dirs_and_pinned_index_dir() {
        let dir = tmp_repo();
        let root = dir.path();
        std::fs::write(root.join("lib.rs"), "fn main() {}\n").unwrap();
        std::fs::create_dir_all(root.join("target")).unwrap();
        std::fs::write(
            root.join("target").join("nested.rs"),
            "fn dont_index() {}\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join(".idx")).unwrap();
        std::fs::write(root.join(".idx").join("cached.rs"), "fn dont_either() {}\n").unwrap();
        let packet = index_repo(root).expect("index");
        assert_eq!(packet.files.len(), 1);
        assert_eq!(packet.files[0].path, "lib.rs");
    }

    #[test]
    fn rust_methods_reach_the_packet_with_their_container() {
        // The gap this closes: a method inside `impl` was invisible to the
        // column-0 scanner, so the digest listed types but none of their API.
        let dir = tmp_repo();
        let root = dir.path();
        std::fs::write(
            root.join("lib.rs"),
            "pub struct Server { port: u16 }\n\nimpl Server {\n    pub fn new(port: u16) -> Self { Self { port } }\n    fn handle(&self) {}\n}\n",
        )
        .unwrap();
        let packet = index_repo(root).expect("index");
        let lib = &packet.files[0];
        let new = lib
            .symbols
            .iter()
            .find(|s| s.name == "new")
            .unwrap_or_else(|| panic!("new missing: {:?}", lib.symbols));
        assert_eq!(new.kind, "method");
        assert_eq!(new.container.as_deref(), Some("Server"));
        assert_eq!(new.line, 4);
        assert!(lib.symbols.iter().any(|s| s.name == "handle"));
    }

    #[test]
    fn find_ranks_exact_before_prefix_before_substring() {
        let dir = tmp_repo();
        let root = dir.path();
        std::fs::write(
            root.join("lib.rs"),
            "fn parse() {}\nfn parse_cidr() {}\nfn parse_cidr_strict() {}\nstruct Parser;\n",
        )
        .unwrap();
        let packet = index_repo(root).expect("index");
        let hits = find_symbols(&packet, "parse", 10);
        let names: Vec<&str> = hits.iter().map(|h| h.name).collect();
        assert_eq!(
            names.first(),
            Some(&"parse"),
            "exact match must lead: {names:?}"
        );
        // Prefix matches come before the substring-only `Parser`.
        let parser_at = names.iter().position(|n| *n == "Parser").expect("Parser");
        let strict_at = names
            .iter()
            .position(|n| *n == "parse_cidr_strict")
            .expect("strict");
        assert!(strict_at < parser_at, "{names:?}");
        assert!(hits.iter().all(|h| h.rank <= 2));
        assert_eq!(hits[0].rank, 0);
    }

    #[test]
    fn find_matches_a_qualified_name_and_respects_the_limit() {
        let dir = tmp_repo();
        let root = dir.path();
        std::fs::write(
            root.join("lib.rs"),
            "struct Sandbox;\nimpl Sandbox {\n    fn from_env() {}\n}\nfn from_env() {}\n",
        )
        .unwrap();
        let packet = index_repo(root).expect("index");
        // `Type::method` resolves even though the packet stores them apart.
        let hits = find_symbols(&packet, "Sandbox::from_env", 10);
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0].container, Some("Sandbox"));
        // The free function of the same name is not a qualified match.
        let all = find_symbols(&packet, "from_env", 10);
        assert_eq!(all.len(), 2, "{all:?}");
        assert_eq!(find_symbols(&packet, "from_env", 1).len(), 1);
        // A miss is an empty result, which `cmd_index` turns into exit 1.
        assert!(find_symbols(&packet, "nowhere", 10).is_empty());
    }

    #[test]
    fn index_go_funcs_and_types() {
        let dir = tmp_repo();
        let root = dir.path();
        std::fs::write(
            root.join("main.go"),
            "import \"fmt\"\n\nfunc main() {}\nfunc (s Server) Start() {}\ntype Server struct{}\n",
        )
        .unwrap();
        let packet = index_repo(root).expect("index");
        let m = &packet.files[0];
        let names: Vec<&str> = m.symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"main"));
        assert!(names.contains(&"Start"));
        assert!(names.contains(&"Server"));
        assert!(m.imports.iter().any(|i| i == "fmt"));
    }
}
