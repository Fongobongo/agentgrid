//! Stage 4.1: `SKILL.md` parser + skill discovery.
//!
//! Minimal YAML-frontmatter parser for the documented schema (flat scalars +
//! simple lists + a flat `metadata` map). Deliberately does NOT pull in a full
//! YAML engine: the SKILL.md contract is small and we want zero new runtime deps.
//!
//! ponytail: flat-schema parser only; nested/complex YAML is downgraded to a
//! warning in lenient mode and an error in strict mode. Swap for `serde_yaml`
//! if real-world skills need nested structures.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

pub mod scanner;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// A parsed `SKILL.md`. `body` is the markdown after the frontmatter and is
/// only materialised on activation (progressive disclosure).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Skill {
    pub name: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compatibility: Option<String>,
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    #[serde(default)]
    pub metadata: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub body: String,
}

#[derive(Debug, Error, PartialEq)]
#[error("skill parse error: {}", errors.join("; "))]
pub struct SkillParseError {
    pub errors: Vec<String>,
}

/// Result of a (possibly lenient) parse: the skill plus any non-fatal warnings.
#[derive(Debug, PartialEq)]
pub struct ParseResult {
    pub skill: Skill,
    pub warnings: Vec<String>,
}

fn strip_quotes(s: &str) -> String {
    let t = s.trim();
    if (t.starts_with('"') && t.ends_with('"') || t.starts_with('\'') && t.ends_with('\''))
        && t.len() >= 2
    {
        t[1..t.len() - 1].to_string()
    } else {
        t.to_string()
    }
}

/// Parse `SKILL.md` content into a [`Skill`].
///
/// `strict` makes structural problems fatal; lenient mode downgrades them to
/// warnings and best-effort continues. Missing `name`/`description` is always
/// fatal (a skill without a name is unusable regardless of mode).
pub fn parse_skill_md(content: &str, strict: bool) -> Result<ParseResult, SkillParseError> {
    // Split into frontmatter text + body.
    let (fm_lines, body) = match extract_frontmatter(content) {
        Ok(v) => v,
        Err(e) => return Err(SkillParseError { errors: vec![e] }),
    };

    let mut scalars: HashMap<String, String> = HashMap::new();
    let mut lists: HashMap<String, Vec<String>> = HashMap::new();
    let mut metadata: HashMap<String, String> = HashMap::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    // Current container context for indented items.
    let mut ctx: Option<String> = None;

    for (idx, raw) in fm_lines.iter().enumerate() {
        let line = *raw;
        if line.trim().is_empty() {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        let trimmed = line.trim();
        if trimmed.starts_with("- ") || trimmed == "-" {
            // list item
            let item = strip_quotes(trimmed.trim_start_matches("- ").trim_start_matches('-'));
            match &ctx {
                Some(k) if k == "metadata" => {
                    // metadata entries are `key: value`, not list items
                    errors.push(format!(
                        "line {}: list item under metadata map is not supported",
                        idx + 1
                    ));
                }
                Some(k) => lists.entry(k.clone()).or_default().push(item),
                None => {
                    let msg = format!("line {}: list item with no parent key", idx + 1);
                    if strict {
                        errors.push(msg);
                    } else {
                        warnings.push(msg);
                    }
                }
            }
            continue;
        }
        if indent > 0 && ctx.as_deref() == Some("metadata") {
            // metadata sub-key
            if let Some((k, v)) = trimmed.split_once(':') {
                metadata.insert(k.trim().to_string(), strip_quotes(v));
            } else {
                let msg = format!("line {}: malformed metadata entry", idx + 1);
                if strict {
                    errors.push(msg);
                } else {
                    warnings.push(msg);
                }
            }
            continue;
        }
        // top-level scalar or container key
        if let Some((k, v)) = trimmed.split_once(':') {
            let key = k.trim().to_string();
            let value = v.trim();
            if value.is_empty() {
                ctx = Some(key.clone());
            } else {
                scalars.insert(key.clone(), strip_quotes(value));
                ctx = None;
            }
        } else {
            let msg = format!("line {}: cannot parse '{}'", idx + 1, trimmed);
            if strict {
                errors.push(msg);
            } else {
                warnings.push(msg);
            }
        }
    }

    // Build the skill.
    let name = scalars.get("name").cloned().unwrap_or_default();
    let description = scalars.get("description").cloned().unwrap_or_default();
    if name.trim().is_empty() {
        errors.push("required field 'name' is missing or empty".into());
    }
    if description.trim().is_empty() {
        errors.push("required field 'description' is missing or empty".into());
    }

    if !errors.is_empty() {
        return Err(SkillParseError { errors });
    }

    let skill = Skill {
        name,
        description,
        license: scalars.get("license").cloned(),
        compatibility: scalars.get("compatibility").cloned(),
        allowed_tools: lists.get("allowed-tools").cloned().unwrap_or_default(),
        metadata,
        body: body.trim_start_matches('\n').to_string(),
    };
    Ok(ParseResult { skill, warnings })
}

/// Split content into (frontmatter lines, body). Frontmatter must start with
/// `---` on the first line and end with the next `---`.
fn extract_frontmatter(content: &str) -> Result<(Vec<&str>, String), String> {
    let lines: Vec<&str> = content.lines().collect();
    if lines.is_empty() || lines[0].trim() != "---" {
        return Err("missing opening '---' frontmatter delimiter".into());
    }
    let mut fm = Vec::new();
    let mut close = None;
    for (i, l) in lines.iter().enumerate().skip(1) {
        if l.trim() == "---" {
            close = Some(i);
            break;
        }
        fm.push(*l);
    }
    let close = match close {
        Some(c) => c,
        None => return Err("missing closing '---' frontmatter delimiter".into()),
    };
    let body = lines[close + 1..].join("\n");
    Ok((fm, body))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SkillSource {
    /// Highest precedence: `<project>/.agents/skills`.
    Project = 0,
    /// User-global: `~/.agents/skills`.
    User = 1,
    /// Managed bundles (lowest precedence).
    Managed = 2,
}

impl SkillSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            SkillSource::Project => "project",
            SkillSource::User => "user",
            SkillSource::Managed => "managed",
        }
    }
}

/// A skill found on disk at `path`, from `source`.
#[derive(Debug, Clone)]
pub struct DiscoveredSkill {
    pub skill: Skill,
    pub source: SkillSource,
    pub path: std::path::PathBuf,
}

/// Discover skills under the given roots (each `(source, root_dir)`).
///
/// Roots are scanned in precedence order (project > user > managed). When the
/// same skill name appears in multiple precedence tiers, the higher-precedence
/// copy wins and a diagnostic is emitted. Returns the resolved skills plus any
/// diagnostics (parse failures, collisions).
pub fn discover(
    roots: &[(SkillSource, std::path::PathBuf)],
) -> (Vec<DiscoveredSkill>, Vec<String>) {
    let mut ordered = roots.to_vec();
    ordered.sort_by_key(|(s, _)| *s);

    let mut resolved: BTreeMap<String, DiscoveredSkill> = BTreeMap::new();
    let mut diagnostics: Vec<String> = Vec::new();

    for (source, root) in ordered {
        let dir = match std::fs::read_dir(&root) {
            Ok(d) => d,
            Err(_) => continue, // missing root is not an error
        };
        for entry in dir.flatten() {
            let skill_dir = entry.path();
            if !skill_dir.is_dir() {
                continue;
            }
            let skill_md = skill_dir.join("SKILL.md");
            let content = match std::fs::read_to_string(&skill_md) {
                Ok(c) => c,
                Err(_) => continue,
            };
            match parse_skill_md(&content, false) {
                Ok(res) => {
                    let name = res.skill.name.clone();
                    let discovered = DiscoveredSkill {
                        skill: res.skill,
                        source,
                        path: skill_md.clone(),
                    };
                    if let Some(existing) = resolved.get(&name) {
                        diagnostics.push(format!(
                            "collision: skill '{}' from {} overridden by higher-precedence {} (kept)",
                            name,
                            source.as_str(),
                            existing.source.as_str()
                        ));
                        // higher precedence already inserted first; keep it
                    } else {
                        resolved.insert(name, discovered);
                    }
                }
                Err(e) => diagnostics.push(format!(
                    "skill at {} failed to parse: {}",
                    skill_md.display(),
                    e
                )),
            }
        }
    }

    (resolved.into_values().collect(), diagnostics)
}

/// Convenience: standard discovery roots for a project dir and an optional
/// user home. Managed bundles are added by the caller.
pub fn standard_roots(
    project_dir: &Path,
    user_home: Option<&Path>,
) -> Vec<(SkillSource, std::path::PathBuf)> {
    let mut roots = vec![(
        SkillSource::Project,
        project_dir.join(".agents").join("skills"),
    )];
    if let Some(home) = user_home {
        roots.push((SkillSource::User, home.join(".agents").join("skills")));
    }
    roots
}

/// Stage 13 (Swarms import): build a [`WorkflowTemplate`](agentgrid_common::WorkflowTemplate)
/// from dependency-aware local skills. Each (collision-resolved) skill becomes
/// one worker step (`Apply skill '<name>': <description>`); edges come from
/// the `depends_on` metadata key (CSV of skill names, e.g.
/// `depends_on: base, helpers`). Unknown or self-referencing deps are an `Err`
/// (fail-closed before DAG validation); the finished template is
/// DAG-validated, so cycles surface here — never on the control plane.
pub fn swarms_to_template(
    name: &str,
    skills: &[DiscoveredSkill],
) -> Result<agentgrid_common::WorkflowTemplate, String> {
    use agentgrid_common::{WorkflowRole, WorkflowStep, WorkflowTemplate};
    let known: std::collections::HashSet<&str> =
        skills.iter().map(|d| d.skill.name.as_str()).collect();
    let mut steps = Vec::with_capacity(skills.len());
    for d in skills {
        let deps: Vec<String> = d
            .skill
            .metadata
            .get("depends_on")
            .map(|csv| {
                csv.split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        for dep in &deps {
            if !known.contains(dep.as_str()) {
                return Err(format!(
                    "skill '{}' depends on unknown skill '{}'",
                    d.skill.name, dep
                ));
            }
            if dep == &d.skill.name {
                return Err(format!("skill '{}' depends on itself", d.skill.name));
            }
        }
        steps.push(WorkflowStep {
            id: d.skill.name.clone(),
            prompt: format!(
                "Apply skill '{}' ({}): {}",
                d.skill.name,
                d.source.as_str(),
                d.skill.description
            ),
            depends_on: deps,
            role: WorkflowRole::Worker,
            adapter: None,
            requested_node_id: None,
            base_commit: None,
            retryable: None,
            max_attempts: None,
            expandable: None,
        });
    }
    let tpl = WorkflowTemplate {
        id: String::new(),
        name: name.to_string(),
        steps,
        budget: None,
        created_at: String::new(),
    };
    tpl.validate_dag()?;
    Ok(tpl)
}

mod bundle;
pub use bundle::*;

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str =
        "---\nname: git-helper\ndescription: Helps with git tasks\n---\nBody text.\n";
    const FULL: &str = "---\nname: db-tool\n\
        description: Database helpers\n\
        license: MIT\n\
        compatibility: opencode>=0.2\n\
        allowed-tools:\n  - Bash\n  - Read\n\
        metadata:\n  tier: 1\n  owner: infra\n\
        ---\n# DB tool\n\nUse with care.\n";

    #[test]
    fn parses_minimal() {
        let r = parse_skill_md(MINIMAL, true).unwrap();
        assert_eq!(r.skill.name, "git-helper");
        assert_eq!(r.skill.description, "Helps with git tasks");
        assert!(r.skill.allowed_tools.is_empty());
        assert_eq!(r.skill.body, "Body text.");
    }

    #[test]
    fn parses_full_with_lists_and_metadata() {
        let r = parse_skill_md(FULL, true).unwrap();
        assert_eq!(r.skill.name, "db-tool");
        assert_eq!(r.skill.license.as_deref(), Some("MIT"));
        assert_eq!(r.skill.compatibility.as_deref(), Some("opencode>=0.2"));
        assert_eq!(r.skill.allowed_tools, vec!["Bash", "Read"]);
        assert_eq!(r.skill.metadata.get("tier").map(String::as_str), Some("1"));
        assert_eq!(
            r.skill.metadata.get("owner").map(String::as_str),
            Some("infra")
        );
        assert!(r.skill.body.contains("Use with care."));
    }

    #[test]
    fn missing_required_is_fatal_in_both_modes() {
        let bad = "---\ndescription: no name\n---\n";
        assert!(parse_skill_md(bad, true).is_err());
        assert!(parse_skill_md(bad, false).is_err());
    }

    #[test]
    fn malformed_yaml_strict_errors_lenient_warns() {
        let bad = "---\nname: x\ndescription: y\n- orphan item\n---\n";
        assert!(parse_skill_md(bad, true).is_err());
        let r = parse_skill_md(bad, false).unwrap();
        assert!(!r.warnings.is_empty());
    }

    #[test]
    fn missing_closing_delimiter_errors() {
        let bad = "---\nname: x\ndescription: y\n";
        assert!(parse_skill_md(bad, true).is_err());
    }

    fn swarm_skill(name: &str, depends_on: Option<&str>) -> DiscoveredSkill {
        let mut metadata = HashMap::new();
        if let Some(d) = depends_on {
            metadata.insert("depends_on".to_string(), d.to_string());
        }
        DiscoveredSkill {
            skill: Skill {
                name: name.to_string(),
                description: format!("does {name}"),
                license: None,
                compatibility: None,
                allowed_tools: vec![],
                metadata,
                body: String::new(),
            },
            source: SkillSource::Project,
            path: std::path::PathBuf::from(format!("/skills/{name}")),
        }
    }

    #[test]
    fn swarms_import_builds_dag_from_depends_on() {
        let skills = vec![
            swarm_skill("base", None),
            swarm_skill("mid", Some("base")),
            swarm_skill("top", Some("base, mid")),
        ];
        let tpl = swarms_to_template("swarm", &skills).unwrap();
        assert_eq!(tpl.name, "swarm");
        assert_eq!(tpl.steps.len(), 3);
        let by_id: HashMap<&str, &agentgrid_common::WorkflowStep> =
            tpl.steps.iter().map(|s| (s.id.as_str(), s)).collect();
        assert!(by_id["mid"].depends_on == vec!["base"]);
        assert!(by_id["top"].depends_on == vec!["base", "mid"]);
        assert!(by_id["top"].prompt.contains("top"));
    }

    #[test]
    fn swarms_import_rejects_unknown_self_and_cyclic_deps() {
        // Unknown dep.
        let e = swarms_to_template("s", &[swarm_skill("a", Some("ghost"))]).unwrap_err();
        assert!(e.contains("unknown skill"), "{e}");
        // Self dep.
        let e = swarms_to_template("s", &[swarm_skill("a", Some("a"))]).unwrap_err();
        assert!(e.contains("itself"), "{e}");
        // Cycle a↔b passes the local checks but fails DAG validation.
        let e = swarms_to_template(
            "s",
            &[swarm_skill("a", Some("b")), swarm_skill("b", Some("a"))],
        )
        .unwrap_err();
        assert!(!e.is_empty());
    }

    #[test]
    fn swarms_import_empty_is_an_empty_template() {
        let tpl = swarms_to_template("empty", &[]).unwrap();
        assert!(tpl.steps.is_empty());
    }
}
