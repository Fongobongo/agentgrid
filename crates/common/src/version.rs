//! Plan 6.9: semver/range comparison for tool version requirements.
//!
//! Hand-rolled, no new dependency (same discipline as the CIDR parser in
//! the node sandbox): covers the requirement forms operators actually
//! type — `*`, exact (`1.2.3`), caret (`^1.2.3`), tilde (`~1.2.3`),
//! comparators (`>=1.2.3`, `>`, `<=`, `<`, `=`), and `x`-wildcards
//! (`1.2.*`, `1.*`). Anything else is not a requirement (the caller falls
//! back to legacy prefix matching, so pins like `"0."` keep working).

/// A parsed `major.minor.patch[-pre]` version. Build metadata (`+…`) is
/// accepted and ignored, per semver §10.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SemVer {
    major: u64,
    minor: u64,
    patch: u64,
    /// Pre-release identifiers (`1.0.0-rc.1` → `["rc", "1"]`), empty = release.
    pre: Vec<String>,
}

impl SemVer {
    fn cmp_release(&self, o: &SemVer) -> std::cmp::Ordering {
        (self.major, self.minor, self.patch).cmp(&(o.major, o.minor, o.patch))
    }
}

/// Parse `v1.2.3`, `1.2`, `1`, with optional `-pre` and `+build`. A missing
/// minor/patch defaults to 0 (`1` → `1.0.0`). Garbage → None.
fn parse_version(s: &str) -> Option<SemVer> {
    let s = s.trim().strip_prefix('v').unwrap_or(s.trim()).trim();
    if s.is_empty() {
        return None;
    }
    // Strip build metadata (ignored for precedence).
    let (s, _) = match s.split_once('+') {
        Some((v, _)) => (v, true),
        None => (s, false),
    };
    let (core, pre) = match s.split_once('-') {
        Some((c, p)) => (c, p),
        None => (s, ""),
    };
    if core.is_empty() || pre.contains('+') {
        return None;
    }
    let mut parts = core.split('.');
    let num = |p: Option<&str>| -> Option<u64> {
        let p = p?;
        // No leading `+`/`-`, no empty, digits only (leading zeros tolerated —
        // strict semver forbids them, but version probes print `01` rarely
        // enough that rejecting would false-negative a compatible tool).
        if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        p.parse().ok()
    };
    let major = num(parts.next())?;
    let minor = num(parts.next()).unwrap_or(0);
    let patch = num(parts.next()).unwrap_or(0);
    if parts.next().is_some() {
        return None; // 4+ numeric components.
    }
    let pre: Vec<String> = if pre.is_empty() {
        Vec::new()
    } else {
        let ids: Vec<String> = pre.split('.').map(str::to_string).collect();
        // Pre-release identifiers are `[0-9A-Za-z-`]+ and non-empty.
        if ids
            .iter()
            .any(|id| id.is_empty() || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'))
        {
            return None;
        }
        ids
    };
    Some(SemVer {
        major,
        minor,
        patch,
        pre,
    })
}

/// Compare pre-release identifiers per semver §11: a release outranks any
/// pre-release of the same triple; identifiers compare numerically when
/// both are numeric, lexically otherwise; a shorter set outranks a longer
/// one when all preceding identifiers are equal.
fn cmp_pre(a: &[String], b: &[String]) -> std::cmp::Ordering {
    match (a.is_empty(), b.is_empty()) {
        (true, true) => std::cmp::Ordering::Equal,
        (true, false) => std::cmp::Ordering::Greater,
        (false, true) => std::cmp::Ordering::Less,
        (false, false) => {
            for (x, y) in a.iter().zip(b.iter()) {
                let ord = match (x.parse::<u64>(), y.parse::<u64>()) {
                    (Ok(nx), Ok(ny)) => nx.cmp(&ny),
                    _ => x.cmp(y),
                };
                if ord != std::cmp::Ordering::Equal {
                    return ord;
                }
            }
            a.len().cmp(&b.len())
        }
    }
}

fn cmp_full(a: &SemVer, b: &SemVer) -> std::cmp::Ordering {
    a.cmp_release(b).then_with(|| cmp_pre(&a.pre, &b.pre))
}

/// True when `version` satisfies the requirement `req`. Requirement forms:
/// `*` (any), exact (`1.2.3`), caret (`^1.2.3` — same-major floor…
/// with the `0.x` rules: `^0.2.3` := `>=0.2.3 <0.3.0`, `^0.0.3` :=
/// `=0.0.3`), tilde (`~1.2.3` := `>=1.2.3 <1.3.0`, `~1.2` := `>=1.2.0
/// <1.3.0`, `~1` := `>=1.0.0 <2.0.0`), comparators (`>=`, `>`, `<=`,
/// `<`, `=`), and wildcards (`1.2.*`, `1.*`, `*`). A leading `v` is
/// tolerated on both sides. Pre-releases match a range only when the
/// requirement names the same `major.minor.patch` with its own
/// pre-release (npm rule, simplified) — otherwise `>=1.0.0` must not
/// admit `2.0.0-rc.1`; exact equality always compares fully.
pub fn version_matches_req(version: &str, req: &str) -> bool {
    let req = req.trim();
    if req.is_empty() || req == "*" {
        return !version.trim().is_empty();
    }
    let v = match parse_version(version) {
        Some(v) => v,
        None => return false,
    };
    // Comparator set: `>=1.2.3 <2.0.0` (space-separated, all must hold).
    if req.contains(' ') || req.starts_with(">=") || req.starts_with("<=") {
        return match_comparators(&v, req);
    }
    if let Some(r) = req.strip_prefix('>') {
        // Strict `>` (the `>=` form is routed to match_comparators above).
        // A lone `>` admits a pre-release only with the same triple.
        let allow = parse_version(r.trim())
            .map(|q| !q.pre.is_empty() && q.cmp_release(&v) == std::cmp::Ordering::Equal)
            .unwrap_or(false);
        return match_single_comparator(&v, r, false, false, allow);
    }
    if let Some(r) = req.strip_prefix('<') {
        let allow = parse_version(r.trim())
            .map(|q| !q.pre.is_empty() && q.cmp_release(&v) == std::cmp::Ordering::Equal)
            .unwrap_or(false);
        return match_single_comparator(&v, r, true, false, allow);
    }
    if let Some(r) = req.strip_prefix('=') {
        return match_exact(&v, r.trim());
    }
    if let Some(r) = req.strip_prefix('^') {
        return match_caret(&v, r.trim());
    }
    if let Some(r) = req.strip_prefix('~') {
        return match_tilde(&v, r.trim());
    }
    if req.contains('*') || req.contains('x') || req.contains('X') {
        return match_wildcard(&v, req);
    }
    match_exact(&v, req)
}

/// Validate a requirement string without a version: true when it is one of
/// the forms [`version_matches_req`] understands (used to reject typos at
/// registration time instead of silently never matching). Empty means
/// "no requirement" to the callers — invalid here so a present-but-empty
/// `version_req` is rejected rather than stored as dead data.
pub fn valid_version_req(req: &str) -> bool {
    let req = req.trim();
    if req.is_empty() || req == "*" {
        return req == "*";
    }
    // Probe against a canary version — validation is "does this parse",
    // not "does it match something".
    if req.contains(' ') || req.starts_with(">=") || req.starts_with("<=") {
        return match_comparators(
            &SemVer {
                major: 1,
                minor: 0,
                patch: 0,
                pre: vec![],
            },
            req,
        ) || parses_as_comparator_set(req);
    }
    let body = req
        .strip_prefix('>')
        .or_else(|| req.strip_prefix('<'))
        .or_else(|| req.strip_prefix('='))
        .or_else(|| req.strip_prefix('^'))
        .or_else(|| req.strip_prefix('~'))
        .unwrap_or(req);
    if req.contains('*') || req.contains('x') || req.contains('X') {
        return wildcard_parses(body);
    }
    parse_version(body).is_some()
}

fn match_exact(v: &SemVer, req: &str) -> bool {
    match parse_version(req) {
        Some(r) => cmp_full(v, &r) == std::cmp::Ordering::Equal,
        None => false,
    }
}

fn match_caret(v: &SemVer, req: &str) -> bool {
    let r = match parse_version(req) {
        Some(r) => r,
        None => return false,
    };
    if cmp_full(v, &r) == std::cmp::Ordering::Less {
        return false;
    }
    // Upper bound: left-most non-zero component pins the range.
    let within = if r.major > 0 {
        v.major == r.major
    } else if r.minor > 0 {
        v.major == 0 && v.minor == r.minor
    } else {
        v.major == 0 && v.minor == 0 && v.patch == r.patch
    };
    within && prerelease_ok(v, &r)
}

fn match_tilde(v: &SemVer, req: &str) -> bool {
    // `~1` and `~1.2` pin the minor range; `~1.2.3` pins patch.
    let dots = req.chars().filter(|&c| c == '.').count();
    let r = match parse_version(req) {
        Some(r) => r,
        None => return false,
    };
    if cmp_release_lt(v, &r) {
        return false;
    }
    let within = if dots == 0 {
        v.major == r.major
    } else {
        v.major == r.major && v.minor == r.minor
    };
    within && prerelease_ok(v, &r)
}

fn cmp_release_lt(v: &SemVer, r: &SemVer) -> bool {
    v.cmp_release(r) == std::cmp::Ordering::Less
}

/// Pre-release gate (npm rule, simplified): a version with a pre-release
/// tag satisfies a range only when the requirement names the same triple
/// with its own pre-release. Exact matches bypass this (handled by
/// [`match_exact`], which compares fully).
fn prerelease_ok(v: &SemVer, r: &SemVer) -> bool {
    if v.pre.is_empty() {
        return true;
    }
    v.cmp_release(r) == std::cmp::Ordering::Equal && !r.pre.is_empty()
}

fn match_wildcard(v: &SemVer, req: &str) -> bool {
    if !wildcard_parses(req) {
        return false;
    }
    let parts: Vec<&str> = req.split('.').collect();
    // `*` alone matches everything (also handled by the caller fast path).
    if parts.len() == 1 {
        return true;
    }
    let wild = |p: &str| p == "*" || p.eq_ignore_ascii_case("x");
    let major: u64 = match parts[0].parse() {
        Ok(n) => n,
        Err(_) => return false,
    };
    if v.major != major {
        return false;
    }
    if parts.len() > 1 && !wild(parts[1]) {
        let minor: u64 = match parts[1].parse() {
            Ok(n) => n,
            Err(_) => return false,
        };
        if v.minor != minor {
            return false;
        }
    }
    if parts.len() > 2 && !wild(parts[2].split('-').next().unwrap_or("")) {
        // A pinned patch (`1.2.3`, possibly with pre-release suffix text —
        // compare numerically only; `1.2.3-rc` as a wildcard is malformed
        // and rejected by `wildcard_parses`).
        let patch: u64 = match parts[2].parse() {
            Ok(n) => n,
            Err(_) => return false,
        };
        if v.patch != patch {
            return false;
        }
    }
    // Wildcards never admit pre-releases (same rationale as ranges).
    v.pre.is_empty()
}

fn wildcard_parses(req: &str) -> bool {
    let parts: Vec<&str> = req.split('.').collect();
    if parts.len() > 3 {
        return false;
    }
    let wild = |p: &str| p == "*" || p.eq_ignore_ascii_case("x");
    // A wildcard must come last: `1.*.3` is malformed.
    let mut seen_wild = false;
    for (i, p) in parts.iter().enumerate() {
        if wild(p) {
            seen_wild = true;
            // A lone `*`/`x` is only valid as the whole requirement here
            // (the caller handles the bare-`*` fast path); `1.*` is fine.
            if i == 0 {
                return false;
            }
        } else if seen_wild || p.parse::<u64>().is_err() {
            // Numeric after a wildcard (`1.*.3`), or non-numeric junk.
            return false;
        }
    }
    // `1.2.3` without any wildcard is not a wildcard requirement.
    seen_wild
}

/// Match one comparator (`op` split out): `gt` = strict-greater side
/// (`>`), else strict-less (`<`). `>=`/`<=` arrive with `or_equal`.
/// Pre-release gating is the CALLER's job (set-level allowance) — except
/// the single-comparator requirement form, which carries its own triple.
fn match_single_comparator(
    v: &SemVer,
    body: &str,
    less: bool,
    or_equal: bool,
    allow_pre: bool,
) -> bool {
    let body = body.trim();
    // `>=`/`<=` handled by match_comparators; a stray `=>`/`=<` is garbage.
    if body.starts_with('=') || body.starts_with('>') || body.starts_with('<') {
        return false;
    }
    let r = match parse_version(body) {
        Some(r) => r,
        None => return false,
    };
    if !v.pre.is_empty() && !allow_pre {
        return false;
    }
    let ord = cmp_full(v, &r);
    if less {
        ord == std::cmp::Ordering::Less || (or_equal && ord == std::cmp::Ordering::Equal)
    } else {
        ord == std::cmp::Ordering::Greater || (or_equal && ord == std::cmp::Ordering::Equal)
    }
}

/// Strip the operator off one comparator part (`>=1.2.3` → `1.2.3`).
fn comparator_body(part: &str) -> &str {
    part.strip_prefix(">=")
        .or_else(|| part.strip_prefix("<="))
        .or_else(|| part.strip_prefix('>'))
        .or_else(|| part.strip_prefix('<'))
        .or_else(|| part.strip_prefix('='))
        .unwrap_or(part)
        .trim()
}

/// npm rule: a pre-release version satisfies a comparator set only when at
/// least one comparator names the same `major.minor.patch` with its own
/// pre-release tag. Releases always pass this gate.
fn set_allows_prerelease(v: &SemVer, req: &str) -> bool {
    if v.pre.is_empty() {
        return true;
    }
    req.split_whitespace().any(|part| {
        parse_version(comparator_body(part))
            .map(|r| !r.pre.is_empty() && r.cmp_release(v) == std::cmp::Ordering::Equal)
            .unwrap_or(false)
    })
}

fn match_comparators(v: &SemVer, req: &str) -> bool {
    if !set_allows_prerelease(v, req) {
        return false;
    }
    let mut ok = true;
    for part in req.split_whitespace() {
        let part_ok = if let Some(r) = part.strip_prefix(">=") {
            match_single_comparator(v, r, false, true, true)
        } else if let Some(r) = part.strip_prefix("<=") {
            match_single_comparator(v, r, true, true, true)
        } else if let Some(r) = part.strip_prefix('>') {
            match_single_comparator(v, r, false, false, true)
        } else if let Some(r) = part.strip_prefix('<') {
            match_single_comparator(v, r, true, false, true)
        } else if let Some(r) = part.strip_prefix('=') {
            match_exact(v, r.trim())
        } else {
            // Bare `1.2.3` inside a set means exact.
            match_exact(v, part)
        };
        ok = ok && part_ok;
        if !ok {
            return false;
        }
    }
    ok
}

/// True when the string parses as a comparator set (every part a comparator
/// or exact version), regardless of whether the canary matches.
fn parses_as_comparator_set(req: &str) -> bool {
    for part in req.split_whitespace() {
        let body = part
            .strip_prefix(">=")
            .or_else(|| part.strip_prefix("<="))
            .or_else(|| part.strip_prefix('>'))
            .or_else(|| part.strip_prefix('<'))
            .or_else(|| part.strip_prefix('='))
            .unwrap_or(part);
        if parse_version(body.trim()).is_none() {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_and_star() {
        assert!(version_matches_req("1.2.3", "1.2.3"));
        assert!(!version_matches_req("1.2.4", "1.2.3"));
        assert!(version_matches_req("1.2.3", "*"));
        assert!(version_matches_req("anything", "*"));
        assert!(
            version_matches_req("v1.2.3", "1.2.3"),
            "leading v tolerated"
        );
        assert!(!version_matches_req("garbage", "1.2.3"));
        assert!(!version_matches_req("", "*"), "empty version never matches");
    }

    #[test]
    fn caret_ranges() {
        assert!(version_matches_req("1.2.3", "^1.2.0"));
        assert!(version_matches_req("1.9.0", "^1.2.0"));
        assert!(!version_matches_req("2.0.0", "^1.2.0"));
        assert!(!version_matches_req("1.1.9", "^1.2.0"));
        // 0.x rules: minor pins below 1.0, patch pins below 0.1.
        assert!(version_matches_req("0.2.5", "^0.2.3"));
        assert!(!version_matches_req("0.3.0", "^0.2.3"));
        assert!(version_matches_req("0.0.3", "^0.0.3"));
        assert!(!version_matches_req("0.0.4", "^0.0.3"));
        // Short forms.
        assert!(version_matches_req("1.5.0", "^1"));
        assert!(!version_matches_req("2.0.0", "^1"));
    }

    #[test]
    fn tilde_ranges() {
        assert!(version_matches_req("1.2.9", "~1.2.3"));
        assert!(!version_matches_req("1.3.0", "~1.2.3"));
        assert!(!version_matches_req("1.2.2", "~1.2.3"));
        assert!(version_matches_req("1.2.0", "~1.2"));
        assert!(!version_matches_req("1.3.0", "~1.2"));
        assert!(version_matches_req("1.9.9", "~1"));
        assert!(!version_matches_req("2.0.0", "~1"));
    }

    #[test]
    fn comparators_and_sets() {
        assert!(version_matches_req("1.2.3", ">=1.0.0"));
        assert!(version_matches_req("1.0.0", ">=1.0.0"));
        assert!(!version_matches_req("0.9.9", ">=1.0.0"));
        assert!(version_matches_req("1.2.3", ">1.2.2"));
        assert!(!version_matches_req("1.2.3", ">1.2.3"));
        assert!(version_matches_req("1.2.3", "<2.0.0"));
        assert!(version_matches_req("1.2.3", "<=1.2.3"));
        assert!(version_matches_req("1.5.0", ">=1.2.0 <2.0.0"));
        assert!(!version_matches_req("2.0.0", ">=1.2.0 <2.0.0"));
        assert!(!version_matches_req("1.1.9", ">=1.2.0 <2.0.0"));
    }

    #[test]
    fn wildcards() {
        assert!(version_matches_req("1.2.3", "1.2.*"));
        assert!(version_matches_req("1.2.9", "1.2.x"));
        assert!(!version_matches_req("1.3.0", "1.2.*"));
        assert!(version_matches_req("1.9.9", "1.*"));
        assert!(!version_matches_req("2.0.0", "1.*"));
        assert!(
            !version_matches_req("1.2.3", "1.*.3"),
            "wildcard must come last"
        );
    }

    #[test]
    fn prerelease_gate() {
        // Ranges never admit a pre-release unless the requirement names the
        // same triple with its own pre-release.
        assert!(!version_matches_req("2.0.0-rc.1", ">=1.0.0"));
        assert!(version_matches_req("2.0.0-rc.1", ">=2.0.0-rc.0 <2.0.0"));
        assert!(!version_matches_req("1.0.0-rc.1", "^1.0.0"));
        // Exact equality compares fully (pre-release included).
        assert!(version_matches_req("1.0.0-rc.1", "1.0.0-rc.1"));
        assert!(!version_matches_req("1.0.0", "1.0.0-rc.1"));
    }

    #[test]
    fn requirement_validation() {
        for ok in [
            "*",
            "1.2.3",
            "^1.2",
            "~1.2.3",
            ">=1.0.0",
            ">=1.2.0 <2.0.0",
            "1.2.*",
            "v2",
            "=3.1.4",
        ] {
            assert!(valid_version_req(ok), "{ok} must validate");
        }
        for bad in ["abc", "1.2.3.4", "=>1.0", "1.*.3", ">= ", "^", "~", ""] {
            assert!(!valid_version_req(bad), "{bad:?} must not validate");
        }
    }
}
