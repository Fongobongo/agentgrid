//! ADR 0029 option F/**exchange**: the gateway mints a real, short-lived,
//! provider-issued token and hands it to the sandbox, which then talks to the
//! provider directly.
//!
//! This is the mode for a consumer that cannot be proxied. `inject` works
//! because the sandbox's traffic goes *through* the gateway — it swaps one
//! header and forwards. A `git push` cannot work that way: the client speaks
//! the git smart-HTTP protocol to the provider itself, and rewriting its
//! credential would mean terminating its TLS (ADR 0029 option D, with a CA in
//! every image). So for those secrets the sandbox must hold something real.
//!
//! What makes that acceptable is that the *provider* bounds it: a GitHub App
//! installation token is one hour and scoped to one repository, where the
//! personal access token it replaces is neither. The gateway never hands out the
//! App key — it holds it, signs a short assertion with it, and exchanges that
//! assertion for a token.
//!
//! The asymmetry with `inject` is load-bearing and is why the daemon refuses
//! exchange under different conditions: a capability is worthless anywhere but
//! through the gateway, so advisory networking is survivable. An exchanged token
//! is real and works from anywhere until it expires, so exchange requires the
//! enforced-egress topology.

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context};

/// GitHub caps an App JWT at 10 minutes and rejects one issued in the future.
/// 60 s of backdating absorbs clock skew on the gateway; 540 s leaves margin.
const JWT_BACKDATE_SECS: u64 = 60;
const JWT_LIFETIME_SECS: u64 = 540;

/// What a provider exchange produced. `expires_in` is the provider's own answer,
/// not our request: the daemon reports it, and a wrong number is worse than the
/// real one.
#[derive(Debug, Clone)]
pub struct Minted {
    pub token: String,
    pub expires_in: u64,
}

/// A PEM that must never be printed. `Gateway`'s `Debug` is hand-written for
/// the same reason; this covers the key *before* it reaches a struct that
/// derives one, so a `{:?}` on a `Provider` cannot leak it either.
#[derive(Clone)]
pub struct PemKey(String);

impl std::fmt::Debug for PemKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted pem>")
    }
}

impl PemKey {
    /// Read once, at startup. A per-mint read would turn "the operator moved the
    /// file" into an attempt failure instead of a refusal to start.
    pub fn read(path: &str) -> anyhow::Result<Self> {
        let pem = std::fs::read_to_string(path)
            .with_context(|| format!("read the exchange key {path}"))?;
        if !pem.contains("PRIVATE KEY") {
            bail!("{path} does not look like a PEM private key");
        }
        Ok(Self(pem))
    }

    fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }
}

#[derive(Clone)]
pub enum Provider {
    /// `github-app`: a signed assertion is exchanged for an installation token,
    /// which GitHub scopes to the installation and expires in an hour.
    GithubApp {
        app_id: u64,
        installation_id: u64,
        key: PemKey,
        /// Overridable so a test can point it at a local fake. The default is
        /// GitHub's public API.
        api: String,
    },
}

impl std::fmt::Debug for Provider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::GithubApp {
                app_id,
                installation_id,
                api,
                ..
            } => f
                .debug_struct("GithubApp")
                .field("app_id", app_id)
                .field("installation_id", installation_id)
                .field("api", api)
                .field("key", &"<redacted pem>")
                .finish(),
        }
    }
}

impl Provider {
    pub fn name(&self) -> &'static str {
        match self {
            Self::GithubApp { .. } => "github-app",
        }
    }

    /// Ask the provider for a fresh token.
    pub async fn mint(&self, http: &reqwest::Client) -> anyhow::Result<Minted> {
        match self {
            Self::GithubApp {
                app_id,
                installation_id,
                key,
                api,
            } => {
                let jwt = app_jwt(*app_id, key)?;
                let url = format!(
                    "{}/app/installations/{installation_id}/access_tokens",
                    api.trim_end_matches('/')
                );
                let resp = http
                    .post(&url)
                    .header("authorization", format!("Bearer {jwt}"))
                    .header("accept", "application/vnd.github+json")
                    .header("x-github-api-version", "2022-11-28")
                    .header("user-agent", "agentgrid-cred-gateway")
                    .send()
                    .await
                    .with_context(|| format!("exchange request to {url} failed"))?;
                let status = resp.status();
                if !status.is_success() {
                    let body = resp.text().await.unwrap_or_default();
                    // GitHub's error text cannot contain the App key — it never
                    // left this process — but it is still truncated so a huge
                    // HTML error page cannot become a log line.
                    bail!(
                        "GitHub refused an installation token for app {app_id} \
                         installation {installation_id}: HTTP {status} {}",
                        truncate(&body, 300)
                    );
                }
                let body: serde_json::Value = resp
                    .json()
                    .await
                    .context("GitHub's token response is not JSON")?;
                let token = body
                    .get("token")
                    .and_then(|t| t.as_str())
                    .filter(|t| !t.is_empty())
                    .context("GitHub's token response has no token")?
                    .to_string();
                Ok(Minted {
                    expires_in: expires_in_from(&body),
                    token,
                })
            }
        }
    }

    /// Best effort, and deliberately not an error: the token expires on its own,
    /// and a failed revoke must not fail an attempt whose work is already done.
    pub async fn revoke(&self, http: &reqwest::Client, token: &str) {
        match self {
            Self::GithubApp { api, .. } => {
                let url = format!("{}/installation/token", api.trim_end_matches('/'));
                match http
                    .delete(&url)
                    .header("authorization", format!("Bearer {token}"))
                    .header("accept", "application/vnd.github+json")
                    .header("user-agent", "agentgrid-cred-gateway")
                    .send()
                    .await
                {
                    Ok(r)
                        if r.status().is_success()
                            || r.status() == reqwest::StatusCode::NOT_FOUND => {}
                    Ok(r) => tracing::warn!(
                        status = %r.status(),
                        "installation token revoke failed; it will expire on its own"
                    ),
                    Err(e) => tracing::warn!(
                        error = %e,
                        "installation token revoke failed; it will expire on its own"
                    ),
                }
            }
        }
    }
}

/// The App assertion: RS256, `iss` is the App id, short-lived.
fn app_jwt(app_id: u64, key: &PemKey) -> anyhow::Result<String> {
    #[derive(serde::Serialize)]
    struct Claims {
        iat: u64,
        exp: u64,
        iss: u64,
    }
    let now = unix_now();
    let claims = Claims {
        iat: now.saturating_sub(JWT_BACKDATE_SECS),
        exp: now + JWT_LIFETIME_SECS,
        iss: app_id,
    };
    let encoding = jsonwebtoken::EncodingKey::from_rsa_pem(key.as_bytes())
        .context("the exchange key is not an RSA private key in PEM form")?;
    Ok(jsonwebtoken::encode(
        &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256),
        &claims,
        &encoding,
    )?)
}

/// Trust the provider's `expires_at` rather than assuming an hour. A parse
/// failure falls back to GitHub's documented hour — a reported number that is
/// roughly right beats refusing a token we already hold.
fn expires_in_from(body: &serde_json::Value) -> u64 {
    let Some(at) = body.get("expires_at").and_then(|v| v.as_str()) else {
        return 3600;
    };
    match chrono::DateTime::parse_from_rfc3339(at) {
        Ok(t) => (t.timestamp() - unix_now() as i64).max(0) as u64,
        Err(_) => 3600,
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    // Byte-slicing a UTF-8 string can split a character; back off to a boundary.
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// `github=github-app;app_id=123;installation_id=456;key=/etc/ag/app.pem`
///
/// `,` separates routes, `;` separates fields within one. `api=` is optional and
/// exists so a test (and a GitHub Enterprise install) can name a different host.
pub fn parse_exchange(spec: &str) -> anyhow::Result<HashMap<String, Provider>> {
    let mut out = HashMap::new();
    for entry in spec.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()) {
        let mut fields = entry.split(';').map(|s| s.trim());
        let head = fields.next().unwrap_or_default();
        let Some((name, provider)) = head.split_once('=') else {
            bail!("exchange route '{entry}' is not 'name=provider;field=value…'");
        };
        let (name, provider) = (name.trim(), provider.trim());
        if name.is_empty() || provider.is_empty() {
            bail!("exchange route '{entry}' has an empty name or provider");
        }
        let mut kv = HashMap::new();
        for field in fields.filter(|f| !f.is_empty()) {
            let Some((k, v)) = field.split_once('=') else {
                bail!("exchange route '{name}': field '{field}' is not key=value");
            };
            kv.insert(k.trim().to_string(), v.trim().to_string());
        }
        let provider = match provider {
            "github-app" => Provider::GithubApp {
                app_id: number(&kv, name, "app_id")?,
                installation_id: number(&kv, name, "installation_id")?,
                key: PemKey::read(required(&kv, name, "key")?)?,
                api: kv
                    .get("api")
                    .cloned()
                    .unwrap_or_else(|| "https://api.github.com".to_string()),
            },
            other => bail!(
                "exchange route '{name}': unknown provider '{other}' \
                 (only 'github-app' exists)"
            ),
        };
        if out.insert(name.to_string(), provider).is_some() {
            bail!("exchange route '{name}' is declared twice");
        }
    }
    Ok(out)
}

fn required<'a>(
    kv: &'a HashMap<String, String>,
    route: &str,
    field: &str,
) -> anyhow::Result<&'a str> {
    kv.get(field)
        .map(|s| s.as_str())
        .filter(|s| !s.is_empty())
        .with_context(|| format!("exchange route '{route}' needs {field}="))
}

fn number(kv: &HashMap<String, String>, route: &str, field: &str) -> anyhow::Result<u64> {
    let raw = required(kv, route, field)?;
    raw.parse::<u64>()
        .with_context(|| format!("exchange route '{route}': {field}='{raw}' is not a number"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_github_app_route_parses() {
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("app.pem");
        std::fs::write(
            &key,
            "-----BEGIN RSA PRIVATE KEY-----\nAAAA\n-----END RSA PRIVATE KEY-----\n",
        )
        .unwrap();
        let spec = format!(
            "github=github-app;app_id=123;installation_id=456;key={}",
            key.display()
        );
        let routes = parse_exchange(&spec).unwrap();
        match &routes["github"] {
            Provider::GithubApp {
                app_id,
                installation_id,
                api,
                ..
            } => {
                assert_eq!(*app_id, 123);
                assert_eq!(*installation_id, 456);
                assert_eq!(api, "https://api.github.com");
            }
        }
    }

    #[test]
    fn the_api_base_can_be_overridden() {
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("app.pem");
        std::fs::write(
            &key,
            "-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----\n",
        )
        .unwrap();
        let spec = format!(
            "github=github-app;app_id=1;installation_id=2;api=http://127.0.0.1:9;key={}",
            key.display()
        );
        let routes = parse_exchange(&spec).unwrap();
        match &routes["github"] {
            Provider::GithubApp { api, .. } => assert_eq!(api, "http://127.0.0.1:9"),
        }
    }

    #[test]
    fn an_unknown_provider_is_refused_by_name() {
        let err = parse_exchange("github=gitlab-app;app_id=1").unwrap_err();
        assert!(err.to_string().contains("gitlab-app"), "{err}");
    }

    #[test]
    fn a_missing_field_is_named() {
        let err = parse_exchange("github=github-app;app_id=1").unwrap_err();
        assert!(err.to_string().contains("installation_id"), "{err}");
    }

    #[test]
    fn a_field_that_is_not_a_number_says_which_one() {
        let err = parse_exchange("github=github-app;app_id=x;installation_id=2").unwrap_err();
        assert!(err.to_string().contains("app_id"), "{err}");
    }

    /// The key is read at startup, so a missing or wrong file is a refusal to
    /// start rather than an attempt that fails an hour later.
    #[test]
    fn a_missing_key_file_is_refused() {
        let err = parse_exchange("github=github-app;app_id=1;installation_id=2;key=/no/such.pem")
            .unwrap_err();
        assert!(err.to_string().contains("/no/such.pem"), "{err}");
    }

    #[test]
    fn a_file_that_is_not_a_pem_key_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("app.pem");
        std::fs::write(&key, "not a key").unwrap();
        let err = parse_exchange(&format!(
            "github=github-app;app_id=1;installation_id=2;key={}",
            key.display()
        ))
        .unwrap_err();
        assert!(err.to_string().contains("PEM"), "{err}");
    }

    #[test]
    fn a_route_declared_twice_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("app.pem");
        std::fs::write(
            &key,
            "-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----\n",
        )
        .unwrap();
        let one = format!(
            "github=github-app;app_id=1;installation_id=2;key={}",
            key.display()
        );
        let err = parse_exchange(&format!("{one},{one}")).unwrap_err();
        assert!(err.to_string().contains("twice"), "{err}");
    }

    /// A `{:?}` anywhere — a panic, a log line — must not print the App key.
    #[test]
    fn debugging_a_provider_does_not_print_the_key() {
        let provider = Provider::GithubApp {
            app_id: 1,
            installation_id: 2,
            key: PemKey("-----BEGIN PRIVATE KEY-----\nSECRET\n".into()),
            api: "https://api.github.com".into(),
        };
        let printed = format!("{provider:?}");
        assert!(!printed.contains("SECRET"), "{printed}");
        assert!(printed.contains("redacted"), "{printed}");
    }

    #[test]
    fn expires_in_follows_the_providers_answer() {
        let body = serde_json::json!({"expires_at": "2030-01-01T00:00:00Z"});
        // Far in the future, so the answer is a large positive number rather
        // than a fixed 3600.
        assert!(expires_in_from(&body) > 3600);
    }

    #[test]
    fn a_response_without_expires_at_falls_back_to_an_hour() {
        assert_eq!(expires_in_from(&serde_json::json!({})), 3600);
        assert_eq!(
            expires_in_from(&serde_json::json!({"expires_at": "nonsense"})),
            3600
        );
    }

    /// An `expires_at` in the past must clamp to zero, not underflow into a
    /// gigantic TTL.
    #[test]
    fn an_expiry_in_the_past_clamps_to_zero() {
        let body = serde_json::json!({"expires_at": "2000-01-01T00:00:00Z"});
        assert_eq!(expires_in_from(&body), 0);
    }

    #[test]
    fn truncation_respects_character_boundaries() {
        let s = "ключ-ключ-ключ";
        let out = truncate(s, 5);
        assert!(out.ends_with('…'), "{out}");
        // Would have panicked on a non-boundary slice.
        assert!(s.starts_with(out.trim_end_matches('…')));
    }
}
