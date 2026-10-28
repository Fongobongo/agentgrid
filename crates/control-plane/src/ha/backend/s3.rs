//! Minimal S3-compatible object storage client (path-style requests, SigV4).
//!
//! Deliberately dependency-light: SigV4 is implemented with `hmac` + `sha2`
//! instead of pulling an SDK. Only the operations the HA data plane needs are
//! exposed: PUT/GET/HEAD/DELETE object and ListObjectsV2. The cluster token
//! never reaches this layer; object credentials are separate from election
//! credentials.

use std::fmt;
use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::Utc;
use hmac::{Hmac, KeyInit, Mac};
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE, HOST};
use reqwest::{Method, StatusCode, Url};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio_util::io::ReaderStream;

use super::super::config::S3Config;

const ALGORITHM: &str = "AWS4-HMAC-SHA256";
const MAX_LIST_PAGES: usize = 20;
const CHANGESET_MAGIC: &[u8; 4] = b"AGCS";
const CHANGESET_VERSION: u8 = 1;

type HmacSha256 = Hmac<Sha256>;

/// One object-storage client per HA runtime. Cloning is cheap (reqwest pool).
#[derive(Clone)]
pub struct S3Client {
    http: reqwest::Client,
    base_url: String,
    bucket: String,
    namespace: String,
    region: String,
    access_key: String,
    secret_key: String,
}

impl fmt::Debug for S3Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("S3Client")
            .field("base_url", &self.base_url)
            .field("bucket", &self.bucket)
            .field("namespace", &self.namespace)
            .field("region", &self.region)
            .field("access_key", &self.access_key)
            .field("secret_key", &"<redacted>")
            .finish()
    }
}

/// Decoded changeset object. The header travels with the bytes so a standby
/// never trusts a filename it did not write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangesetObject {
    pub epoch: u64,
    pub schema_version: i64,
    pub first_seq: u64,
    pub last_seq: u64,
    pub bytes: Vec<u8>,
}

#[derive(Serialize, Deserialize)]
struct ChangesetHeader {
    v: u8,
    epoch: u64,
    schema_version: i64,
    first_seq: u64,
    last_seq: u64,
}

impl S3Client {
    pub fn new(cfg: &S3Config, cluster_id: &str, allow_plaintext: bool) -> Result<Self> {
        let endpoint = Url::parse(&cfg.endpoint)
            .with_context(|| format!("invalid AGENTGRID_HA_S3_ENDPOINT {:?}", cfg.endpoint))?;
        if !allow_plaintext && endpoint.scheme() != "https" {
            bail!("AGENTGRID_HA_S3_ENDPOINT must use https (set AGENTGRID_HA_ALLOW_PLAINTEXT=1 for tests)");
        }
        if !matches!(endpoint.scheme(), "http" | "https") || endpoint.host_str().is_none() {
            bail!("AGENTGRID_HA_S3_ENDPOINT must be an http(s) URL with a host");
        }
        if cfg.bucket.trim().is_empty() {
            bail!("AGENTGRID_HA_S3_BUCKET must not be empty");
        }
        let base_url = cfg.endpoint.trim_end_matches('/').to_string();
        let namespace = format!(
            "{}/{}",
            cfg.prefix.trim_matches('/'),
            cluster_id.trim_matches('/')
        )
        .trim_matches('/')
        .to_string();
        let http = reqwest::Client::builder()
            // A redirect could replay signed credentials to another host.
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(60))
            .build()?;
        Ok(Self {
            http,
            base_url,
            bucket: cfg.bucket.trim().to_string(),
            namespace,
            region: cfg.region.trim().to_string(),
            access_key: cfg.access_key.clone(),
            secret_key: cfg.secret_key.clone(),
        })
    }

    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    fn object_key(&self, key: &str) -> String {
        format!("{}/{key}", self.namespace)
    }

    fn snapshot_key(&self, id: &str, suffix: &str) -> String {
        self.object_key(&format!("snapshots/{id}.{suffix}"))
    }

    fn latest_snapshot_key(&self) -> String {
        self.object_key("snapshots/latest.json")
    }

    fn changeset_key(&self, first_seq: u64, last_seq: u64) -> String {
        self.object_key(&format!("changesets/{first_seq:020}-{last_seq:020}.bin"))
    }

    fn artifact_key(&self, sha256: &str) -> Result<String> {
        if sha256.len() != 64 || !sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
            bail!("refusing an invalid artifact sha256");
        }
        Ok(self.object_key(&format!("artifacts/{}", sha256.to_ascii_lowercase())))
    }

    /// PUT object bytes.
    pub async fn put(&self, key: &str, bytes: &[u8], content_type: &str) -> Result<()> {
        let url = self.object_url(Some(key), &[])?;
        let hash = sha256_hex(bytes);
        let req = self
            .signed(Method::PUT, &url, &[], &hash)?
            .header(CONTENT_TYPE, content_type)
            .header(CONTENT_LENGTH, bytes.len())
            .body(bytes.to_vec());
        self.send(req, "PUT", key).await?;
        Ok(())
    }

    /// PUT a file without loading it into RAM.
    pub async fn put_file(&self, key: &str, path: &Path, content_type: &str) -> Result<()> {
        let (sha, size) = {
            let p = path.to_path_buf();
            tokio::task::spawn_blocking(move || sha256_file(&p))
                .await
                .context("S3 file hash task panicked")??
        };
        let file = tokio::fs::File::open(path)
            .await
            .with_context(|| format!("open {} for S3 upload", path.display()))?;
        let stream = ReaderStream::new(file);
        let url = self.object_url(Some(key), &[])?;
        let req = self
            .signed(Method::PUT, &url, &[], &sha)?
            .header(CONTENT_TYPE, content_type)
            .header(CONTENT_LENGTH, size)
            .body(reqwest::Body::wrap_stream(stream));
        self.send(req, "PUT", key).await?;
        Ok(())
    }

    /// GET object bytes. `None` means the object does not exist.
    pub async fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let url = self.object_url(Some(key), &[])?;
        let req = self.signed(Method::GET, &url, &[], &sha256_hex(b""))?;
        let resp = req.send().await.context("S3 GET request")?;
        if resp.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !resp.status().is_success() {
            return Err(http_error("GET", key, resp).await);
        }
        Ok(Some(resp.bytes().await?.to_vec()))
    }

    /// Stream an object into `dest`; the caller verifies size and hash.
    pub async fn download_to_file(&self, key: &str, dest: &Path) -> Result<u64> {
        let url = self.object_url(Some(key), &[])?;
        let req = self.signed(Method::GET, &url, &[], &sha256_hex(b""))?;
        let mut resp = req.send().await.context("S3 snapshot GET")?;
        if !resp.status().is_success() {
            return Err(http_error("GET", key, resp).await);
        }
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent).await.ok();
        }
        let mut file = tokio::fs::File::create(dest).await?;
        let mut total = 0u64;
        while let Some(chunk) = resp.chunk().await? {
            total += chunk.len() as u64;
            tokio::io::AsyncWriteExt::write_all(&mut file, &chunk).await?;
        }
        tokio::io::AsyncWriteExt::flush(&mut file).await?;
        file.sync_all().await?;
        Ok(total)
    }

    /// DELETE is used only for explicit maintenance; a missing object is fine.
    pub async fn delete(&self, key: &str) -> Result<()> {
        let url = self.object_url(Some(key), &[])?;
        let req = self.signed(Method::DELETE, &url, &[], &sha256_hex(b""))?;
        let resp = req.send().await.context("S3 DELETE request")?;
        if resp.status().is_success() || resp.status() == StatusCode::NOT_FOUND {
            return Ok(());
        }
        Err(http_error("DELETE", key, resp).await)
    }

    /// List keys under `prefix` (relative to this cluster's namespace).
    pub async fn list(&self, prefix: &str) -> Result<Vec<String>> {
        self.list_from(prefix, None).await
    }

    /// List keys under `prefix`, starting strictly after `start_after` (a key
    /// relative to this cluster's namespace). ListObjectsV2 `start-after` is
    /// what keeps a standby's changeset poll proportional to the *new*
    /// batches instead of the whole prefix.
    pub async fn list_from(&self, prefix: &str, start_after: Option<&str>) -> Result<Vec<String>> {
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        for _ in 0..MAX_LIST_PAGES {
            let mut query: Vec<(String, String)> = vec![
                ("list-type".into(), "2".into()),
                ("prefix".into(), self.object_key(prefix)),
                ("max-keys".into(), "1000".into()),
            ];
            if let Some(after) = start_after {
                query.push(("start-after".into(), self.object_key(after)));
            }
            if let Some(t) = token.as_deref() {
                query.push(("continuation-token".into(), t.to_string()));
            }
            let url = self.object_url(None, &query)?;
            let req = self.signed(Method::GET, &url, &query, &sha256_hex(b""))?;
            let resp = req.send().await.context("S3 LIST request")?;
            if !resp.status().is_success() {
                return Err(http_error("LIST", prefix, resp).await);
            }
            let body = resp.text().await?;
            for key in xml_values(&body, "Key") {
                out.push(xml_unescape(&key));
            }
            token = xml_values(&body, "NextContinuationToken")
                .into_iter()
                .next()
                .map(|t| xml_unescape(&t));
            if !xml_values(&body, "IsTruncated")
                .first()
                .is_some_and(|v| v.eq_ignore_ascii_case("true"))
            {
                break;
            }
            if token.is_none() {
                break;
            }
        }
        out.sort();
        Ok(out)
    }

    /// Upload a snapshot database file plus its manifest and the advisory
    /// `latest.json` pointer.
    pub async fn put_snapshot(&self, id: &str, db_path: &Path, manifest: &[u8]) -> Result<()> {
        let db_key = self.snapshot_key(id, "db");
        self.put_file(&db_key, db_path, "application/octet-stream")
            .await?;
        let manifest_key = self.snapshot_key(id, "json");
        self.put(&manifest_key, manifest, "application/json")
            .await?;
        self.put(&self.latest_snapshot_key(), manifest, "application/json")
            .await?;
        Ok(())
    }

    pub async fn latest_snapshot_manifest(&self) -> Result<Option<Vec<u8>>> {
        self.get(&self.latest_snapshot_key()).await
    }

    pub async fn download_snapshot(&self, id: &str, dest: &Path) -> Result<u64> {
        self.download_to_file(&self.snapshot_key(id, "db"), dest)
            .await
    }

    pub async fn put_changeset(
        &self,
        epoch: u64,
        schema_version: i64,
        first_seq: u64,
        last_seq: u64,
        bytes: &[u8],
    ) -> Result<()> {
        let header = ChangesetHeader {
            v: CHANGESET_VERSION,
            epoch,
            schema_version,
            first_seq,
            last_seq,
        };
        let header = serde_json::to_vec(&header)?;
        if header.len() > u32::MAX as usize {
            bail!("changeset header too large");
        }
        let mut body = Vec::with_capacity(4 + 4 + header.len() + bytes.len());
        body.extend_from_slice(CHANGESET_MAGIC);
        body.extend_from_slice(&(header.len() as u32).to_be_bytes());
        body.extend_from_slice(&header);
        body.extend_from_slice(bytes);
        let key = self.changeset_key(first_seq, last_seq);
        self.put(&key, &body, "application/octet-stream").await
    }

    /// Keys for committed changeset batches, oldest first.
    pub async fn list_changesets(&self) -> Result<Vec<String>> {
        let keys = self.list("changesets/").await?;
        Ok(keys.into_iter().filter(|k| k.ends_with(".bin")).collect())
    }

    /// Changeset keys strictly newer than `after_seq`.
    ///
    /// Keys are zero-padded (`changesets/<first>-<last>.bin`), so a plain
    /// `start-after` of the bare sequence number is both correct and cheap:
    /// a standby that is already at `after_seq` no longer re-lists every batch
    /// ever written (the old behaviour grew O(batches) at one poll per 2 s).
    pub async fn list_changesets_after(&self, after_seq: u64) -> Result<Vec<String>> {
        let start = format!("changesets/{after_seq:020}");
        let keys = self.list_from("changesets/", Some(&start)).await?;
        Ok(keys.into_iter().filter(|k| k.ends_with(".bin")).collect())
    }

    /// Delete changeset batches whose whole range is at or below `below_seq`,
    /// keeping the newest `keep` of them. Returns how many objects were
    /// removed.
    ///
    /// `below_seq` is the sequence a snapshot already contains, so nothing
    /// deleted here is needed to reconstruct that state; `keep` is the safety
    /// margin for a standby that is mid-catch-up (it falls back to the
    /// snapshot if it still hits a gap).
    pub async fn prune_changesets(&self, below_seq: u64, keep: usize) -> Result<usize> {
        let mut eligible: Vec<(u64, u64, String)> = self
            .list_changesets()
            .await?
            .into_iter()
            .filter_map(|key| {
                let (first, last) = Self::changeset_range(&key)?;
                (last <= below_seq).then_some((first, last, key))
            })
            .collect();
        eligible.sort();
        let drop_count = eligible.len().saturating_sub(keep);
        let mut deleted = 0usize;
        for (_, _, key) in eligible.into_iter().take(drop_count) {
            self.delete(&key).await?;
            deleted += 1;
        }
        Ok(deleted)
    }

    /// Sequence range encoded in a changeset object key. Returns `None` for a
    /// key written by a different/foreign producer.
    pub fn changeset_range(key: &str) -> Option<(u64, u64)> {
        let name = key.rsplit('/').next()?.strip_suffix(".bin")?;
        let (first, last) = name.split_once('-')?;
        Some((first.parse().ok()?, last.parse().ok()?))
    }

    pub async fn get_changeset(&self, key: &str) -> Result<Option<ChangesetObject>> {
        let Some(body) = self.get(key).await? else {
            return Ok(None);
        };
        Ok(Some(decode_changeset(&body)?))
    }

    pub async fn put_artifact(&self, sha256: &str, bytes: &[u8]) -> Result<()> {
        let key = self.artifact_key(sha256)?;
        self.put(&key, bytes, "application/octet-stream").await
    }

    pub async fn get_artifact(&self, sha256: &str) -> Result<Option<Vec<u8>>> {
        let key = self.artifact_key(sha256)?;
        self.get(&key).await
    }

    fn object_url(&self, key: Option<&str>, query: &[(String, String)]) -> Result<Url> {
        let mut url = format!("{}/{}", self.base_url, uri_encode(&self.bucket));
        if let Some(key) = key {
            for segment in key.split('/') {
                url.push('/');
                url.push_str(&uri_encode(segment));
            }
        }
        if !query.is_empty() {
            url.push('?');
            url.push_str(&canonical_query(query));
        }
        Url::parse(&url).with_context(|| format!("build S3 URL for {:?}", key))
    }

    fn signed(
        &self,
        method: Method,
        url: &Url,
        query: &[(String, String)],
        payload_hash: &str,
    ) -> Result<reqwest::RequestBuilder> {
        let now = Utc::now();
        let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
        let headers = self.sign_at(method.as_str(), url, query, payload_hash, &amz_date)?;
        Ok(self.http.request(method, url.clone()).headers(headers))
    }

    fn sign_at(
        &self,
        method: &str,
        url: &Url,
        query: &[(String, String)],
        payload_hash: &str,
        amz_date: &str,
    ) -> Result<HeaderMap> {
        let date = &amz_date[..8];
        let host = host_header(url);
        let canonical_headers =
            format!("host:{host}\nx-amz-content-sha256:{payload_hash}\nx-amz-date:{amz_date}\n");
        let signed_headers = "host;x-amz-content-sha256;x-amz-date";
        let canonical_request = format!(
            "{method}\n{}\n{}\n{canonical_headers}\n{signed_headers}\n{payload_hash}",
            url.path(),
            canonical_query(query)
        );
        let scope = format!("{date}/{}/s3/aws4_request", self.region);
        let string_to_sign = format!(
            "{ALGORITHM}\n{amz_date}\n{scope}\n{}",
            sha256_hex(canonical_request.as_bytes())
        );
        let k_date = hmac_sha256(
            format!("AWS4{}", self.secret_key).as_bytes(),
            date.as_bytes(),
        )?;
        let k_region = hmac_sha256(&k_date, self.region.as_bytes())?;
        let k_service = hmac_sha256(&k_region, b"s3")?;
        let k_signing = hmac_sha256(&k_service, b"aws4_request")?;
        let signature = hex::encode(hmac_sha256(&k_signing, string_to_sign.as_bytes())?);
        let authorization = format!(
            "{ALGORITHM} Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
            self.access_key
        );

        let mut headers = HeaderMap::new();
        headers.insert(HOST, HeaderValue::from_str(&host)?);
        headers.insert("x-amz-content-sha256", HeaderValue::from_str(payload_hash)?);
        headers.insert("x-amz-date", HeaderValue::from_str(amz_date)?);
        headers.insert(AUTHORIZATION, HeaderValue::from_str(&authorization)?);
        Ok(headers)
    }

    async fn send(
        &self,
        req: reqwest::RequestBuilder,
        what: &str,
        key: &str,
    ) -> Result<reqwest::Response> {
        let resp = req
            .send()
            .await
            .with_context(|| format!("S3 {what} request"))?;
        if !resp.status().is_success() {
            return Err(http_error(what, key, resp).await);
        }
        Ok(resp)
    }

    #[cfg(test)]
    fn sign_for_test(
        &self,
        method: &str,
        url: &Url,
        payload_hash: &str,
        amz_date: &str,
    ) -> Result<HeaderMap> {
        self.sign_at(method, url, &[], payload_hash, amz_date)
    }
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> Result<Vec<u8>> {
    let mut mac = HmacSha256::new_from_slice(key).context("HMAC key")?;
    mac.update(data);
    Ok(mac.finalize().into_bytes().to_vec())
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn sha256_file(path: &Path) -> Result<(String, u64)> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)
        .with_context(|| format!("open {} for hashing", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    let mut size = 0u64;
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        size += n as u64;
        hasher.update(&buf[..n]);
    }
    Ok((hex::encode(hasher.finalize()), size))
}

fn host_header(url: &Url) -> String {
    let host = url.host_str().unwrap_or_default();
    match (url.scheme(), url.port()) {
        ("http", Some(80)) | ("https", Some(443)) | (_, None) => host.to_string(),
        (_, Some(port)) => format!("{host}:{port}"),
    }
}

/// SigV4 byte-wise percent encoding. Slashes are encoded here; callers split
/// keys into segments first so object path separators survive.
fn uri_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn canonical_query(query: &[(String, String)]) -> String {
    let mut pairs: Vec<(String, String)> = query
        .iter()
        .map(|(k, v)| (uri_encode(k), uri_encode(v)))
        .collect();
    pairs.sort();
    pairs
        .into_iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

fn xml_values(body: &str, tag: &str) -> Vec<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut out = Vec::new();
    let mut rest = body;
    while let Some(start) = rest.find(&open) {
        let after = &rest[start + open.len()..];
        let Some(end) = after.find(&close) else {
            break;
        };
        out.push(after[..end].to_string());
        rest = &after[end + close.len()..];
    }
    out
}

fn xml_unescape(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
}

fn decode_changeset(body: &[u8]) -> Result<ChangesetObject> {
    if body.len() < 8 || &body[..4] != CHANGESET_MAGIC {
        bail!("not an agentgrid changeset object");
    }
    let hlen = u32::from_be_bytes(body[4..8].try_into().expect("4 bytes")) as usize;
    let end = 8usize
        .checked_add(hlen)
        .filter(|e| *e <= body.len())
        .ok_or_else(|| anyhow::anyhow!("truncated changeset header"))?;
    let header: ChangesetHeader = serde_json::from_slice(&body[8..end])?;
    if header.v != CHANGESET_VERSION {
        bail!("unsupported changeset object version {}", header.v);
    }
    if header.first_seq > header.last_seq {
        bail!("changeset sequence range is inverted");
    }
    Ok(ChangesetObject {
        epoch: header.epoch,
        schema_version: header.schema_version,
        first_seq: header.first_seq,
        last_seq: header.last_seq,
        bytes: body[end..].to_vec(),
    })
}

async fn http_error(what: &str, key: &str, resp: reqwest::Response) -> anyhow::Error {
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    let body: String = body.chars().take(512).collect();
    anyhow::anyhow!("S3 {what} {key} failed: HTTP {status}: {body}")
}

/// In-process S3 mock for unit/integration tests. Supports the subset the HA
/// data plane uses: PUT/GET/HEAD/DELETE and ListObjectsV2.
#[cfg(test)]
pub mod mock {
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    use axum::body::Bytes;
    use axum::extract::{Path, Query, State};
    use axum::http::{header, StatusCode};
    use axum::response::{IntoResponse, Response};
    use axum::routing::get;
    use axum::Router;
    use serde::Deserialize;

    #[derive(Clone, Default)]
    pub struct MockS3 {
        objects: Arc<Mutex<BTreeMap<String, Vec<u8>>>>,
    }

    impl MockS3 {
        pub async fn start() -> (Self, String) {
            let state = Self::default();
            let app = Router::new()
                .route("/{bucket}", get(list))
                .route(
                    "/{bucket}/{*key}",
                    get(get_object)
                        .put(put_object)
                        .head(head_object)
                        .delete(delete_object),
                )
                .with_state(state.clone());
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("mock s3 bind");
            let addr = listener.local_addr().expect("mock s3 addr");
            tokio::spawn(async move {
                let _ = axum::serve(listener, app).await;
            });
            (state, format!("http://{addr}"))
        }

        pub fn get(&self, key: &str) -> Option<Vec<u8>> {
            self.objects.lock().expect("mock s3 lock").get(key).cloned()
        }

        pub fn keys(&self) -> Vec<String> {
            self.objects
                .lock()
                .expect("mock s3 lock")
                .keys()
                .cloned()
                .collect()
        }

        pub fn len(&self) -> usize {
            self.objects.lock().expect("mock s3 lock").len()
        }

        pub fn is_empty(&self) -> bool {
            self.len() == 0
        }
    }

    fn object_key(bucket: &str, rest: &str) -> String {
        format!("{bucket}/{rest}")
    }

    async fn put_object(
        State(s): State<MockS3>,
        Path((bucket, key)): Path<(String, String)>,
        headers: axum::http::HeaderMap,
        body: Bytes,
    ) -> Response {
        // Reject unsigned requests so a broken signer cannot silently pass.
        if !headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("AWS4-HMAC-SHA256 "))
        {
            return StatusCode::FORBIDDEN.into_response();
        }
        let body = body.to_vec();
        let expected = headers
            .get("x-amz-content-sha256")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        if expected != agentgrid_common::sha256_hex(&body) {
            return StatusCode::BAD_REQUEST.into_response();
        }
        s.objects
            .lock()
            .expect("mock s3 lock")
            .insert(object_key(&bucket, &key), body);
        StatusCode::OK.into_response()
    }

    async fn get_object(
        State(s): State<MockS3>,
        Path((bucket, key)): Path<(String, String)>,
    ) -> Response {
        match s
            .objects
            .lock()
            .expect("mock s3 lock")
            .get(&object_key(&bucket, &key))
            .cloned()
        {
            Some(bytes) => (
                [
                    (header::CONTENT_TYPE, "application/octet-stream"),
                    (
                        header::HeaderName::from_static("x-amz-content-sha256"),
                        "mock",
                    ),
                ],
                bytes,
            )
                .into_response(),
            None => StatusCode::NOT_FOUND.into_response(),
        }
    }

    async fn head_object(
        State(s): State<MockS3>,
        Path((bucket, key)): Path<(String, String)>,
    ) -> Response {
        if s.objects
            .lock()
            .expect("mock s3 lock")
            .contains_key(&object_key(&bucket, &key))
        {
            StatusCode::OK.into_response()
        } else {
            StatusCode::NOT_FOUND.into_response()
        }
    }

    async fn delete_object(
        State(s): State<MockS3>,
        Path((bucket, key)): Path<(String, String)>,
    ) -> Response {
        s.objects
            .lock()
            .expect("mock s3 lock")
            .remove(&object_key(&bucket, &key));
        StatusCode::NO_CONTENT.into_response()
    }

    #[derive(Deserialize)]
    struct ListQuery {
        prefix: Option<String>,
        #[serde(rename = "continuation-token")]
        _continuation_token: Option<String>,
        #[serde(rename = "start-after")]
        start_after: Option<String>,
    }

    async fn list(
        State(s): State<MockS3>,
        Path(bucket): Path<String>,
        Query(q): Query<ListQuery>,
    ) -> Response {
        let prefix = format!("{bucket}/{}", q.prefix.unwrap_or_default());
        let start_after = q.start_after.map(|k| format!("{bucket}/{k}"));
        let objects = s.objects.lock().expect("mock s3 lock");
        let mut xml = String::from(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
             <ListBucketResult><IsTruncated>false</IsTruncated>",
        );
        let mut keys: Vec<&String> = objects
            .keys()
            .filter(|k| k.starts_with(&prefix))
            .filter(|k| start_after.as_ref().is_none_or(|after| *k > after))
            .collect();
        keys.sort();
        for key in keys {
            let rest = key.strip_prefix(&format!("{bucket}/")).unwrap_or(key);
            xml.push_str(&format!(
                "<Contents><Key>{}</Key></Contents>",
                xml_escape(rest)
            ));
        }
        xml.push_str("</ListBucketResult>");
        ([(header::CONTENT_TYPE, "application/xml")], xml).into_response()
    }

    fn xml_escape(s: &str) -> String {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    }
}

#[cfg(test)]
mod tests {
    use super::mock::MockS3;
    use super::*;
    use crate::ha::config::S3Config;

    fn cfg(endpoint: String) -> S3Config {
        S3Config {
            endpoint,
            bucket: "agentgrid".into(),
            region: "us-east-1".into(),
            prefix: "ha".into(),
            access_key: "AKIAIOSFODNN7EXAMPLE".into(),
            secret_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into(),
        }
    }

    #[test]
    fn debug_redacts_the_secret() {
        let c = cfg("https://s3.example.com".into());
        let client = S3Client::new(&c, "prod", false).expect("client");
        let dbg = format!("{client:?}");
        assert!(dbg.contains("<redacted>"));
        assert!(!dbg.contains(&c.secret_key));
    }

    #[test]
    fn rejects_plaintext_endpoint_without_opt_in() {
        let c = cfg("http://127.0.0.1:9000".into());
        assert!(S3Client::new(&c, "prod", false).is_err());
        assert!(S3Client::new(&c, "prod", true).is_ok());
    }

    #[test]
    fn sigv4_headers_have_a_stable_shape() {
        let c = cfg("https://s3.example.com".into());
        let client = S3Client::new(&c, "prod", false).expect("client");
        let url = client
            .object_url(Some("snapshots/ha-snap-1-2.db"), &[])
            .expect("url");
        let headers = client
            .sign_for_test("PUT", &url, &sha256_hex(b"hello"), "20130524T000000Z")
            .expect("sign");
        let auth = headers
            .get(AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .expect("authorization");
        assert!(auth.starts_with(
            "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request,"
        ));
        assert!(auth.contains("SignedHeaders=host;x-amz-content-sha256;x-amz-date"));
        assert_eq!(
            headers.get("x-amz-content-sha256").unwrap(),
            &HeaderValue::from_str(&sha256_hex(b"hello")).unwrap()
        );
    }

    #[tokio::test]
    async fn put_get_list_delete_round_trip() {
        let (mock, endpoint) = MockS3::start().await;
        let client = S3Client::new(&cfg(endpoint), "prod", true).expect("client");
        client
            .put("ha/prod/artifacts/abc", b"payload", "text/plain")
            .await
            .expect("put");
        assert_eq!(
            client.get("ha/prod/artifacts/abc").await.unwrap(),
            Some(b"payload".to_vec())
        );
        assert!(client
            .get("ha/prod/artifacts/missing")
            .await
            .unwrap()
            .is_none());
        assert!(client
            .list("artifacts/")
            .await
            .unwrap()
            .contains(&"ha/prod/artifacts/abc".to_string()));
        client
            .delete("ha/prod/artifacts/abc")
            .await
            .expect("delete");
        assert!(mock.is_empty());
    }

    #[tokio::test]
    async fn changeset_envelope_round_trips() {
        let (mock, endpoint) = MockS3::start().await;
        let client = S3Client::new(&cfg(endpoint), "prod", true).expect("client");
        client
            .put_changeset(7, 94, 11, 12, b"changes")
            .await
            .expect("put changeset");
        let keys = client.list_changesets().await.expect("list");
        assert_eq!(keys.len(), 1);
        let obj = client
            .get_changeset(&keys[0])
            .await
            .expect("get")
            .expect("exists");
        assert_eq!(
            obj,
            ChangesetObject {
                epoch: 7,
                schema_version: 94,
                first_seq: 11,
                last_seq: 12,
                bytes: b"changes".to_vec(),
            }
        );
        assert_eq!(mock.len(), 1);
    }

    #[tokio::test]
    async fn list_changesets_after_skips_applied_batches() {
        let (_mock, endpoint) = MockS3::start().await;
        let client = S3Client::new(&cfg(endpoint), "prod", true).expect("client");
        for (first, last) in [(1u64, 3u64), (4, 6), (7, 9)] {
            client
                .put_changeset(1, 94, first, last, b"x")
                .await
                .expect("put changeset");
        }
        assert_eq!(client.list_changesets().await.unwrap().len(), 3);

        let after = client.list_changesets_after(6).await.expect("list");
        assert_eq!(after.len(), 1, "only the batch above the applied seq");
        assert!(after[0].ends_with("-00000000000000000009.bin"));

        // Caught up: the poll must return nothing instead of the whole prefix.
        assert!(client
            .list_changesets_after(9)
            .await
            .expect("list")
            .is_empty());
    }

    #[tokio::test]
    async fn prune_changesets_keeps_the_newest_margin() {
        let (mock, endpoint) = MockS3::start().await;
        let client = S3Client::new(&cfg(endpoint), "prod", true).expect("client");
        for batch in 1u64..=5 {
            client
                .put_changeset(1, 94, batch * 2 - 1, batch * 2, b"x")
                .await
                .expect("put changeset");
        }

        // Batches (1,2) and (3,4) are inside the snapshot at seq 4 and beyond
        // the retention margin.
        assert_eq!(client.prune_changesets(4, 0).await.expect("prune"), 2);
        assert_eq!(mock.len(), 3);

        // Keeping more than is eligible deletes nothing.
        assert_eq!(client.prune_changesets(10, 10).await.expect("prune"), 0);
        assert_eq!(mock.len(), 3);

        // Margin of one: everything at or below seq 10 except the newest.
        assert_eq!(client.prune_changesets(10, 1).await.expect("prune"), 2);
        let left = client.list_changesets().await.expect("list");
        assert_eq!(left.len(), 1);
        assert!(left[0].ends_with("-00000000000000000010.bin"));
    }
}
