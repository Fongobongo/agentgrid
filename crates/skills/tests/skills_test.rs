//! Integration tests for the SKILL.md parser + discovery using fixtures.
use std::path::PathBuf;

use agentgrid_skills::{discover, materialize, parse_skill_md, SkillSource, TrustStore};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

#[test]
fn fixture_minimal_parses() {
    let content = std::fs::read_to_string(fixture("minimal/SKILL.md")).unwrap();
    let r = parse_skill_md(&content, true).unwrap();
    assert_eq!(r.skill.name, "git-helper");
    assert_eq!(r.skill.description, "Helps with git tasks");
    assert_eq!(r.skill.body, "Body text.");
}

#[test]
fn fixture_full_parses_lists_and_metadata() {
    let content = std::fs::read_to_string(fixture("full/SKILL.md")).unwrap();
    let r = parse_skill_md(&content, true).unwrap();
    assert_eq!(r.skill.allowed_tools, vec!["Bash", "Read"]);
    assert_eq!(r.skill.metadata.get("tier").map(String::as_str), Some("1"));
    assert_eq!(
        r.skill.metadata.get("owner").map(String::as_str),
        Some("infra")
    );
}

#[test]
fn fixture_malformed_strict_fails_lenient_warns() {
    let content = std::fs::read_to_string(fixture("malformed/SKILL.md")).unwrap();
    assert!(parse_skill_md(&content, true).is_err());
    let r = parse_skill_md(&content, false).unwrap();
    assert!(!r.warnings.is_empty());
}

#[test]
fn fixture_untrusted_script_parses() {
    // Trust gating is Stage 4.2; here we only assert the parser accepts it.
    let content = std::fs::read_to_string(fixture("untrusted-script/SKILL.md")).unwrap();
    let r = parse_skill_md(&content, true).unwrap();
    assert_eq!(r.skill.name, "installer");
    assert!(r.skill.body.contains("curl"));
}

#[test]
fn discovery_resolves_collision_by_precedence() {
    let roots = vec![
        (SkillSource::Project, fixture("collision/project")),
        (SkillSource::User, fixture("collision/user")),
    ];
    let (skills, diagnostics) = discover(&roots);
    assert_eq!(skills.len(), 1);
    let s = &skills[0];
    assert_eq!(s.skill.name, "a");
    assert_eq!(s.source, SkillSource::Project);
    assert_eq!(s.skill.description, "project version");
    assert!(diagnostics.iter().any(|d| d.contains("collision")));
}

#[test]
fn untrusted_project_skill_not_materialized() {
    // Regression: a project (repo-supplied) skill — even a malicious one that
    // pipes a download straight into a shell — must NOT reach the agent unless
    // an operator has explicitly trusted it.
    let root = std::env::temp_dir().join(format!(
        "ag-skill-root-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&root);
    let skill_dir = root.join("installer");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::copy(
        fixture("untrusted-script/SKILL.md"),
        skill_dir.join("SKILL.md"),
    )
    .unwrap();

    let (skills, _diag) = discover(&[(SkillSource::Project, root.clone())]);
    assert_eq!(skills.len(), 1);
    assert!(skills[0].skill.body.contains("curl"));

    let dest = std::env::temp_dir().join(format!(
        "ag-skill-test-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dest);

    // Untrusted: skipped, never written.
    let empty = TrustStore::new();
    let (written, skipped) = materialize(&skills, &dest, &empty, None).unwrap();
    assert!(
        written.is_empty(),
        "untrusted project skill must not be materialized"
    );
    assert_eq!(skipped, vec!["installer".to_string()]);

    // Trusted by an operator: now it materializes.
    let mut trusted = TrustStore::new();
    trusted.trust(SkillSource::Project, "installer");
    let (written2, skipped2) = materialize(&skills, &dest, &trusted, None).unwrap();
    assert_eq!(written2.len(), 1);
    assert!(skipped2.is_empty());

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&dest);
}
