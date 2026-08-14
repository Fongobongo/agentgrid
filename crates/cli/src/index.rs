//! Plan 1.13: `ag index` — offline ctags-like extraction so an agent without
//! built-in codebase awareness can see top-level symbols and imports for a
//! repo before its first attempt. Output is a commit-scoped JSON packet.
//!
//! Lazy by design: line-prefix detection (no AST), top-level signatures only.
//! ponytail: nested defs/inline structs are skipped — the agent needs entry
//! points (fns, types, imports) to orient, not every inner block. Upgrade to
//! tree-sitter only if this heuristic demonstrably falls short on real repos.

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
    pub kind: &'static str, // "fn" | "type" | "const"
    pub name: String,
    pub line: usize,
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

/// Map extension to a language tag, or None for un-indexable files.
fn lang_of(p: &Path) -> Option<&'static str> {
    match p
        .extension()
        .and_then(|e| e.to_str())?
        .to_ascii_lowercase()
        .as_str()
    {
        "rs" => Some("rust"),
        "ts" => Some("typescript"),
        "tsx" => Some("tsx"),
        "js" | "mjs" | "cjs" => Some("javascript"),
        "jsx" => Some("jsx"),
        "py" | "pyi" => Some("python"),
        "go" => Some("go"),
        "java" | "kt" => Some("java"),
        "c" | "h" => Some("c"),
        "cpp" | "cc" | "cxx" | "hpp" | "hh" => Some("cpp"),
        _ => None,
    }
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
            if let Some(lang) = lang_of(&path) {
                if let Ok(src) = fs::read_to_string(&path) {
                    let (symbols, imports) = scan(lang, &src);
                    let rel = path
                        .strip_prefix(root)
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|_| path.display().to_string());
                    let file_indexed =
                        !symbols.is_empty() || !imports.is_empty() || !is_likely_binary_name(&name);
                    if file_indexed {
                        out.push(FileIndex {
                            path: rel,
                            lang,
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

fn is_likely_binary_name(_name: &str) -> bool {
    false
}

/// Line-prefix regex-free extraction: split off signature by leading keyword.
/// Avoids a regex dependency; pattern-table is small and auditable. Only top-
/// level (col 0) signatures are indexed, since indent already implies nested.
fn scan(lang: &'static str, src: &str) -> (Vec<Sym>, Vec<String>) {
    let mut syms = Vec::new();
    let mut imps = Vec::new();
    for (i, line) in src.lines().enumerate() {
        let l = line.trim_start();
        let line_no = i + 1;
        // Top-level only: original line must not be indented.
        let top = !line.starts_with(' ') && !line.starts_with('\t');
        match lang {
            "rust" => scan_rust(line, l, top, line_no, &mut syms, &mut imps),
            "typescript" | "tsx" | "javascript" | "jsx" => {
                scan_js(line, l, top, line_no, &mut syms, &mut imps)
            }
            "python" => scan_python(line, l, top, line_no, &mut syms, &mut imps),
            "go" => scan_go(line, l, top, line_no, &mut syms, &mut imps),
            "java" | "c" | "cpp" => scan_c_family(line, l, top, line_no, &mut syms, &mut imps),
            _ => {}
        }
    }
    (syms, imps)
}

fn take_after_kw<'a>(l: &'a str, kw: &str) -> Option<&'a str> {
    let rest = l.strip_prefix(kw)?.trim_start();
    Some(rest)
}

/// Pull identifier (alphanumeric/underscore, leading alpha/_). Returns ("") on
/// none. Generic params stripped at `<`.
fn ident_of(s: &str) -> String {
    s.split(|c: char| c == '<' || c == '(' || c == ':' || c == '=' || c.is_whitespace())
        .next()
        .unwrap_or("")
        .trim()
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect()
}

fn ident_after(l: &str, kw: &str) -> Option<String> {
    let rest = take_after_kw(l, kw)?;
    let id = ident_of(rest);
    if id.is_empty() {
        None
    } else {
        Some(id)
    }
}

fn scan_rust(
    _line: &str,
    l: &str,
    top: bool,
    line_no: usize,
    syms: &mut Vec<Sym>,
    imps: &mut Vec<String>,
) {
    if !top {
        // Still harvest imports below top-level (rare but possible after
        // conditional compile) — uncommon enough that pruning them is fine.
        return;
    }
    if let Some(n) = ident_after(l, "pub async fn").or_else(|| ident_after(l, "async fn")) {
        syms.push(Sym {
            kind: "fn",
            name: n,
            line: line_no,
        });
    } else if let Some(n) = ident_after(l, "pub fn").or_else(|| ident_after(l, "fn")) {
        syms.push(Sym {
            kind: "fn",
            name: n,
            line: line_no,
        });
    } else if let Some(n) = ident_after(l, "pub struct").or_else(|| ident_after(l, "struct")) {
        syms.push(Sym {
            kind: "type",
            name: n,
            line: line_no,
        });
    } else if let Some(n) = ident_after(l, "pub enum").or_else(|| ident_after(l, "enum")) {
        syms.push(Sym {
            kind: "type",
            name: n,
            line: line_no,
        });
    } else if let Some(n) = ident_after(l, "pub trait").or_else(|| ident_after(l, "trait")) {
        syms.push(Sym {
            kind: "trait",
            name: n,
            line: line_no,
        });
    } else if let Some(n) = ident_after(l, "pub const").or_else(|| ident_after(l, "const")) {
        let _ = n; // consts noisy — keep types/fns only
    } else if let Some(rest) = l.strip_prefix("use ") {
        let after = rest;
        let path = after.split(';').next().unwrap_or(after).trim();
        if !path.is_empty() {
            imps.push(path.to_string());
        }
    }
}

fn scan_js(
    _line: &str,
    l: &str,
    top: bool,
    line_no: usize,
    syms: &mut Vec<Sym>,
    imps: &mut Vec<String>,
) {
    if top {
        if let Some(n) =
            ident_after(l, "export async function").or_else(|| ident_after(l, "async function"))
        {
            syms.push(Sym {
                kind: "fn",
                name: n,
                line: line_no,
            });
        } else if let Some(n) =
            ident_after(l, "export function").or_else(|| ident_after(l, "function"))
        {
            syms.push(Sym {
                kind: "fn",
                name: n,
                line: line_no,
            });
        } else if let Some(n) = ident_after(l, "export class").or_else(|| ident_after(l, "class")) {
            syms.push(Sym {
                kind: "type",
                name: n,
                line: line_no,
            });
        } else if let Some(n) = ident_after(l, "export const").or_else(|| ident_after(l, "const")) {
            // Only index top-level named consts in TS (.d.ts-like surface),
            // since const = noise in JS apps. Still skipped here: piggy on
            // fn/class/type for cross-lang consistency.
            let _ = n;
        }
    }
    if l.starts_with("import ") {
        // Best-effort: capture source path (`from "..."`)
        let from = l.find("from ");
        if let Some(i) = from {
            let rest = &l[i + 5..].trim_start();
            let src = rest
                .trim_start_matches(['"', '\''])
                .split(['"', '\''])
                .next()
                .unwrap_or("");
            if !src.is_empty() {
                imps.push(src.to_string());
            }
        } else {
            // bare side-effect import `import "./x"` or `import * as ns`
            let rest = l.trim_start_matches("import").trim_start();
            let src = rest
                .trim_start_matches(['"', '\''])
                .split(['"', '\''])
                .next()
                .unwrap_or("");
            if !src.is_empty() {
                imps.push(src.to_string());
            }
        }
    }
}

fn scan_python(
    _line: &str,
    l: &str,
    top: bool,
    line_no: usize,
    syms: &mut Vec<Sym>,
    imps: &mut Vec<String>,
) {
    if !top {
        return;
    }
    if let Some(n) = ident_after(l, "async def").or_else(|| ident_after(l, "def")) {
        syms.push(Sym {
            kind: "fn",
            name: n,
            line: line_no,
        });
    } else if let Some(n) = ident_after(l, "class") {
        syms.push(Sym {
            kind: "type",
            name: n,
            line: line_no,
        });
    } else if l.starts_with("import ") || l.starts_with("from ") {
        // from X import Y → "X"; import A, B → "A", "B"
        if let Some(rest) = l.strip_prefix("from ") {
            let modname = rest.split_whitespace().next().unwrap_or("");
            if !modname.is_empty() {
                imps.push(modname.to_string());
            }
        } else if let Some(rest) = l.strip_prefix("import ") {
            for n in rest.split(',') {
                let n = n.split_whitespace().next().unwrap_or("");
                if !n.is_empty() {
                    imps.push(n.to_string());
                }
            }
        }
    }
}

fn scan_go(
    _line: &str,
    l: &str,
    top: bool,
    line_no: usize,
    syms: &mut Vec<Sym>,
    imps: &mut Vec<String>,
) {
    if top {
        if let Some(rest) = l.strip_prefix("func ") {
            // func Name(...) or func (recv) Name(...)
            let after_recv = if rest.starts_with('(') {
                // skip receiver: find matching ')'
                let close = rest.find(')').map(|i| i + 1).unwrap_or(rest.len());
                rest[close..].trim_start()
            } else {
                rest
            };
            let name = ident_of(after_recv);
            if !name.is_empty() {
                syms.push(Sym {
                    kind: "fn",
                    name,
                    line: line_no,
                });
            }
        } else if let Some(n) = ident_after(l, "type") {
            syms.push(Sym {
                kind: "type",
                name: n,
                line: line_no,
            });
        }
    }
    if l.starts_with("import ") {
        // import "x" | import (\nmulti\n)
        let rest = l.strip_prefix("import").unwrap_or(l).trim_start();
        let s = rest.trim_start_matches(['"', '\'']);
        let src = s.split(['"', '\'']).next().unwrap_or("");
        if !src.is_empty() {
            imps.push(src.to_string());
        }
    } else if l.starts_with("\t\"") || l.starts_with("    \"") {
        // Inside a grouped import block.
        let s = l.trim_start();
        let src = s.trim_start_matches('"').split('"').next().unwrap_or("");
        if !src.is_empty() {
            imps.push(src.to_string());
        }
    }
}

fn scan_c_family(
    _line: &str,
    l: &str,
    top: bool,
    line_no: usize,
    syms: &mut Vec<Sym>,
    imps: &mut Vec<String>,
) {
    if top {
        // Best-effort: function-name then arg-list on line-ending-`;` or `{`.
        if let Some(n) = ident_after(l, "static") {
            if !(n == "const" || n == "void") {
                let _ = n; // skip — too noisy in C-family
            }
        }
    }
    // include "x.h"
    if l.starts_with("#include ") || l.starts_with("#import ") {
        let rest = l
            .trim_start_matches(|c: char| c == '#' || c.is_whitespace())
            .trim_start_matches("include")
            .trim_start()
            .trim_start_matches("import")
            .trim_start();
        // Strip <...> or "..." paths
        let src = rest
            .trim_start_matches(['<', '"', '\''])
            .split(['>', '"', '\''])
            .next()
            .unwrap_or("")
            .trim();
        if !src.is_empty() {
            imps.push(src.to_string());
        }
    }
    let _ = (top, line_no, syms);
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
