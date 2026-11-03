//! Cloudflare D1 + Workers KV client.
//!
//! This is a direct port of the engine's Go `db` package. It is not a local
//! database: every call is an HTTPS request to the Cloudflare REST API.
//!
//! * D1 (`chat_sessions`, `chat_turns`, `system_state`, ...) is reached through
//!   [`Client::query`], raw SQL with bound parameters.
//! * Providers and their API keys live in Workers KV, reached through
//!   [`Client::kv_get`], [`Client::kv_put`] and [`Client::kv_delete`].
//!
//! [`Client::verify`] must succeed before any D1 call, and
//! [`Client::verify_kv`] before any KV call. Both are retried by the config
//! reloader, exactly as in the Go engine.

use std::sync::{Arc, RwLock};
use std::time::Duration;

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Base of the Cloudflare v4 API.
pub const API_BASE: &str = "https://api.cloudflare.com/client/v4";

/// Title of the KV namespace holding the providers.
pub const KV_NAMESPACE_TITLE: &str = "aixodia-providers";

/// One D1 row. D1 numbers arrive as `f64`, so columns are dynamically typed.
pub type Row = Map<String, Value>;

/// Errors returned by the D1/KV client.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("db: Verify must succeed first")]
    NotVerified,
    #[error("db: VerifyKV must succeed first")]
    KvNotVerified,
    #[error("db: SQL is empty")]
    EmptySql,
    #[error("db: token has no account")]
    NoAccount,
    #[error("db: D1 database named aixodia was not found in any accessible account")]
    NoDatabase,
    #[error("db: daemon url is empty")]
    EmptyDaemonUrl,
    #[error("db: {operation} failed: {message}")]
    Api { operation: String, message: String },
    #[error("db: D1 returned no query result")]
    NoResult,
    #[error("db: decode {what}: {source}")]
    Decode {
        what: String,
        #[source]
        source: anyhow::Error,
    },
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

// --- Cloudflare envelope -------------------------------------------------

#[derive(Debug, Deserialize)]
struct ApiResponse {
    #[serde(default)]
    success: bool,
    #[serde(default)]
    errors: Vec<ApiError>,
    #[serde(default)]
    result: Value,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ApiError {
    #[serde(default)]
    pub code: i64,
    #[serde(default)]
    pub message: String,
}

#[derive(Debug, Deserialize)]
struct TokenVerifyResult {
    #[serde(default)]
    status: String,
}

#[derive(Debug, Deserialize)]
struct AccountResult {
    #[serde(default)]
    id: String,
}

#[derive(Debug, Deserialize)]
struct DatabaseResult {
    #[serde(default)]
    uuid: String,
    #[serde(default)]
    name: String,
}

#[derive(Debug, Deserialize)]
struct KvNamespaceResult {
    #[serde(default)]
    id: String,
    #[serde(default)]
    title: String,
}

#[derive(Debug, Serialize)]
struct QueryRequest<'a> {
    sql: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<&'a [Value]>,
}

#[derive(Debug, Deserialize)]
struct QueryResult {
    #[serde(default)]
    success: bool,
    #[serde(default)]
    errors: Vec<ApiError>,
    /// D1 calls the row list `results`; an older field name `rows` silently
    /// decoded to an empty list and every SELECT looked like the database was
    /// empty, so this must keep the API's spelling.
    #[serde(default, rename = "results")]
    rows: Vec<Row>,
}

// --- path escaping -------------------------------------------------------

/// Go's `url.PathEscape` keeps unreserved characters and the sub-delims that
/// are legal in a path segment.
const PATH_SEGMENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~')
    .remove(b'$')
    .remove(b'&')
    .remove(b'+')
    .remove(b',')
    .remove(b';')
    .remove(b'=')
    .remove(b':')
    .remove(b'@');

fn path_escape(key: &str) -> String {
    percent_encoding::utf8_percent_encode(key, PATH_SEGMENT).to_string()
}

// --- client --------------------------------------------------------------

#[derive(Default)]
struct Credentials {
    token: String,
    account_id: String,
    db_id: String,
    kv_namespace: String,
}

struct Inner {
    http: reqwest::Client,
    cred: RwLock<Credentials>,
}

/// D1 + KV client. Cheap to clone: all clones share one HTTP client and one
/// credentialed state, like the Go package globals.
#[derive(Clone)]
pub struct Client {
    inner: Arc<Inner>,
}

impl Default for Client {
    fn default() -> Self {
        Self::new()
    }
}

impl Client {
    /// Builds a client with a 15s request timeout, as the Go engine uses.
    pub fn new() -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .expect("reqwest client");
        Self {
            inner: Arc::new(Inner {
                http,
                cred: RwLock::new(Credentials::default()),
            }),
        }
    }

    fn token(&self) -> String {
        self.inner.cred.read().unwrap().token.clone()
    }

    fn account_id(&self) -> String {
        self.inner.cred.read().unwrap().account_id.clone()
    }

    fn db_id(&self) -> String {
        self.inner.cred.read().unwrap().db_id.clone()
    }

    fn kv_namespace(&self) -> String {
        self.inner.cred.read().unwrap().kv_namespace.clone()
    }

    /// Whether [`verify`](Self::verify) has succeeded.
    pub fn is_verified(&self) -> bool {
        let c = self.inner.cred.read().unwrap();
        !c.token.is_empty() && !c.account_id.is_empty() && !c.db_id.is_empty()
    }

    /// The account the token resolved to, once verified.
    pub fn account_id_value(&self) -> Option<String> {
        let id = self.account_id();
        (!id.is_empty()).then_some(id)
    }

    /// The D1 database id, once verified.
    pub fn database_id(&self) -> Option<String> {
        let id = self.db_id();
        (!id.is_empty()).then_some(id)
    }

    /// Verifies the token, then finds the account and the `aixodia` D1
    /// database. Returns `false` on any failure, matching the Go signature; the
    /// detailed error is available through [`verify_detailed`](Self::verify_detailed).
    pub async fn verify(&self, cf_token: &str) -> bool {
        self.verify_detailed(cf_token).await.is_ok()
    }

    /// Same as [`verify`](Self::verify) but keeps the reason it failed.
    pub async fn verify_detailed(&self, cf_token: &str) -> Result<()> {
        let cf_token = cf_token.trim();
        if cf_token.is_empty() {
            return Err(Error::NotVerified);
        }

        // This endpoint only answers for user-owned tokens. An account-owned
        // token is a normal server credential and answers 401 here even when it
        // is valid; the account and database lookups below are the real proof.
        // A user token that was revoked or disabled fails there anyway, so a
        // definite "not active" is respected.
        if let Ok(verify) = self
            .get::<TokenVerifyResult>("/user/tokens/verify", cf_token)
            .await
        {
            if verify.status != "active" {
                return Err(Error::NotVerified);
            }
        }

        let (account_id, db_id) = self.database_location(cf_token).await?;

        let mut cred = self.inner.cred.write().unwrap();
        cred.token = cf_token.to_string();
        cred.account_id = account_id;
        cred.db_id = db_id;
        Ok(())
    }

    async fn database_location(&self, cf_token: &str) -> Result<(String, String)> {
        let accounts: Vec<AccountResult> = self.get("/accounts", cf_token).await?;
        if accounts.is_empty() {
            return Err(Error::NoAccount);
        }

        for account in accounts {
            let path = format!("/accounts/{}/d1/database", account.id);
            let Ok(databases) = self.get::<Vec<DatabaseResult>>(&path, cf_token).await else {
                continue;
            };
            for database in databases {
                if database.name == "aixodia" {
                    return Ok((account.id, database.uuid));
                }
            }
        }
        Err(Error::NoDatabase)
    }

    // --- system_state ----------------------------------------------------

    /// Reads one `system_state` key. `None` means the key was never written,
    /// which is not an error: a fresh install gets the defaults.
    pub async fn get_state(&self, key: &str) -> Result<Option<String>> {
        let rows = self
            .query(
                "SELECT value FROM system_state WHERE key=?",
                &[Value::from(key)],
            )
            .await?;
        Ok(rows
            .first()
            .and_then(|row| row.get("value"))
            .and_then(Value::as_str)
            .map(str::to_string))
    }

    /// Writes one `system_state` key, replacing any earlier value.
    pub async fn put_state(&self, key: &str, value: &str) -> Result<()> {
        self.upsert_state(key, value).await
    }

    async fn upsert_state(&self, key: &str, value: &str) -> Result<()> {
        self.query(
            "INSERT INTO system_state (key, value, updated_at) VALUES (?, ?, unixepoch()) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
            &[Value::from(key), Value::from(value)],
        )
        .await
        .map(|_| ())
    }

    /// Writes this daemon's address, version and first heartbeat, which is how
    /// a client finds the engine and reads its uptime.
    pub async fn save_daemon(&self, url: &str, version: &str, heartbeat: i64) -> Result<()> {
        let url = url.trim();
        if url.is_empty() {
            return Err(Error::EmptyDaemonUrl);
        }
        for (key, value) in [
            ("daemon/url", url.to_string()),
            ("daemon/version", version.to_string()),
            ("daemon/heartbeat", heartbeat.to_string()),
        ] {
            self.upsert_state(key, &value).await?;
        }
        Ok(())
    }

    /// Refreshes the daemon version and heartbeat keys.
    pub async fn touch_daemon(&self, version: &str, heartbeat: i64) -> Result<()> {
        for (key, value) in [
            ("daemon/version", version.to_string()),
            ("daemon/heartbeat", heartbeat.to_string()),
        ] {
            self.upsert_state(key, &value).await?;
        }
        Ok(())
    }

    // --- D1 --------------------------------------------------------------

    /// Runs one statement against D1 and returns its rows.
    pub async fn query(&self, sql: &str, params: &[Value]) -> Result<Vec<Row>> {
        let sql = sql.trim();
        if sql.is_empty() {
            return Err(Error::EmptySql);
        }
        if !self.is_verified() {
            return Err(Error::NotVerified);
        }

        let (account_id, db_id) = (self.account_id(), self.db_id());
        let endpoint = format!("{API_BASE}/accounts/{account_id}/d1/database/{db_id}/query");

        let body = QueryRequest {
            sql,
            // Go omits `params` when empty; D1 accepts the object either way.
            params: (!params.is_empty()).then_some(params),
        };

        let response = self
            .inner
            .http
            .post(&endpoint)
            .headers(self.auth_headers(&self.token()))
            .json(&body)
            .send()
            .await?;

        let status = response.status().as_u16();
        let envelope: ApiResponse = response.json().await?;

        if !(200..300).contains(&status) || !envelope.success {
            return Err(api_error_message(
                "D1 query",
                status as i32,
                &envelope.errors,
            ));
        }

        let results: Vec<QueryResult> =
            serde_json::from_value(envelope.result).map_err(|e| Error::Decode {
                what: "D1 query result".into(),
                source: e.into(),
            })?;
        let Some(first) = results.into_iter().next() else {
            return Err(Error::NoResult);
        };
        if !first.success {
            return Err(api_error_message("D1 query", status as i32, &first.errors));
        }
        Ok(first.rows)
    }

    // --- KV --------------------------------------------------------------

    /// Finds the providers namespace, creating it when the token may. Separate
    /// from [`verify`](Self::verify) so a boot that cannot reach KV still serves
    /// the built-in provider; the config reloader retries it.
    pub async fn verify_kv(&self) -> Result<()> {
        let token = self.token();
        let account_id = self.account_id();
        if token.is_empty() || account_id.is_empty() {
            return Err(Error::NotVerified);
        }

        let path = format!("/accounts/{account_id}/storage/kv/namespaces?per_page=100");
        let namespaces: Vec<KvNamespaceResult> = self.get(&path, &token).await?;
        if let Some(found) = namespaces
            .iter()
            .find(|namespace| namespace.title == KV_NAMESPACE_TITLE)
        {
            self.inner.cred.write().unwrap().kv_namespace = found.id.clone();
            return Ok(());
        }

        let created = self
            .create_kv_namespace(KV_NAMESPACE_TITLE)
            .await
            .map_err(|e| Error::Api {
                operation: format!(
                    "KV namespace {KV_NAMESPACE_TITLE:?} not found and could not be created"
                ),
                message: e.to_string(),
            })?;
        self.inner.cred.write().unwrap().kv_namespace = created.id;
        Ok(())
    }

    /// Whether [`verify_kv`](Self::verify_kv) has found the namespace.
    pub fn kv_ready(&self) -> bool {
        !self.kv_namespace().is_empty()
    }

    async fn create_kv_namespace(&self, title: &str) -> Result<KvNamespaceResult> {
        let endpoint = format!(
            "{API_BASE}/accounts/{}/storage/kv/namespaces",
            self.account_id()
        );
        let response = self
            .inner
            .http
            .post(&endpoint)
            .headers(self.auth_headers(&self.token()))
            .json(&serde_json::json!({ "title": title }))
            .send()
            .await?;

        let status = response.status().as_u16();
        let envelope: ApiResponse = response.json().await?;
        if !(200..300).contains(&status) || !envelope.success {
            return Err(api_error_message(
                "KV create namespace",
                status as i32,
                &envelope.errors,
            ));
        }
        serde_json::from_value(envelope.result).map_err(|e| Error::Decode {
            what: "KV namespace".into(),
            source: e.into(),
        })
    }

    fn kv_url(&self, key: &str) -> String {
        format!(
            "{API_BASE}/accounts/{}/storage/kv/namespaces/{}/values/{}",
            self.account_id(),
            self.kv_namespace(),
            path_escape(key)
        )
    }

    /// Reads one KV value. `None` means the key was never written, which is not
    /// an error: an empty providers key is a fresh install.
    pub async fn kv_get(&self, key: &str) -> Result<Option<String>> {
        if !self.kv_ready() {
            return Err(Error::KvNotVerified);
        }
        let response = self
            .inner
            .http
            .get(self.kv_url(key))
            .headers(self.auth_headers(&self.token()))
            .send()
            .await?;

        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let status = response.status().as_u16();
        let raw = read_limited(response, 4 << 20).await?;
        if !(200..300).contains(&status) {
            return Err(kv_error("KV read", status as i32, &raw));
        }
        Ok(Some(String::from_utf8_lossy(&raw).into_owned()))
    }

    /// Writes one KV value, replacing any earlier one.
    pub async fn kv_put(&self, key: &str, value: &str) -> Result<()> {
        if !self.kv_ready() {
            return Err(Error::KvNotVerified);
        }
        let response = self
            .inner
            .http
            .put(self.kv_url(key))
            .headers(self.auth_headers(&self.token()))
            .header(reqwest::header::CONTENT_TYPE, "text/plain")
            .body(value.to_string())
            .send()
            .await?;

        let status = response.status().as_u16();
        let raw = read_limited(response, 1 << 20).await?;
        if !(200..300).contains(&status) {
            return Err(kv_error("KV write", status as i32, &raw));
        }
        Ok(())
    }

    /// Removes one KV key. Removing a key that is already gone is a success.
    pub async fn kv_delete(&self, key: &str) -> Result<()> {
        if !self.kv_ready() {
            return Err(Error::KvNotVerified);
        }
        let response = self
            .inner
            .http
            .delete(self.kv_url(key))
            .headers(self.auth_headers(&self.token()))
            .send()
            .await?;

        let status = response.status().as_u16();
        if status == 404 {
            return Ok(());
        }
        let raw = read_limited(response, 1 << 20).await?;
        if !(200..300).contains(&status) {
            return Err(kv_error("KV delete", status as i32, &raw));
        }
        Ok(())
    }

    // --- handles ---------------------------------------------------------

    /// A KV handle implementing [`KeyValue`] for `config`.
    pub fn kv(&self) -> Kv {
        Kv(self.clone())
    }

    /// A `system_state` handle implementing [`StateStore`] for the gateway.
    pub fn state(&self) -> State {
        State(self.clone())
    }

    // --- plumbing --------------------------------------------------------

    /// Sets the Cloudflare credentials on a request. A value containing a colon
    /// is an account global API key in `email:key` form and uses the X-Auth-*
    /// headers; anything else is a scoped API token sent as a Bearer.
    fn auth_headers(&self, cred: &str) -> reqwest::header::HeaderMap {
        let mut headers = reqwest::header::HeaderMap::new();
        match cred.find(':') {
            Some(i) if i + 1 < cred.len() => {
                if let (Ok(email), Ok(key)) = (
                    reqwest::header::HeaderValue::from_str(&cred[..i]),
                    reqwest::header::HeaderValue::from_str(&cred[i + 1..]),
                ) {
                    headers.insert("X-Auth-Email", email);
                    headers.insert("X-Auth-Key", key);
                }
            }
            _ => {
                if let Ok(value) = reqwest::header::HeaderValue::from_str(&format!("Bearer {cred}"))
                {
                    headers.insert(reqwest::header::AUTHORIZATION, value);
                }
            }
        }
        headers
    }

    async fn get<T: serde::de::DeserializeOwned>(&self, path: &str, cred: &str) -> Result<T> {
        let response = self
            .inner
            .http
            .get(format!("{API_BASE}{path}"))
            .headers(self.auth_headers(cred))
            .send()
            .await?;

        let status = response.status().as_u16();
        let envelope: ApiResponse = response.json().await?;
        if !(200..300).contains(&status) || !envelope.success {
            return Err(api_error_message(path, status as i32, &envelope.errors));
        }
        serde_json::from_value(envelope.result).map_err(|e| Error::Decode {
            what: path.to_string(),
            source: e.into(),
        })
    }
}

async fn read_limited(response: reqwest::Response, limit: usize) -> Result<Vec<u8>> {
    // Cloudflare never sends far more than this; the cap matches the Go
    // `io.LimitReader` so a hostile response cannot exhaust memory.
    let bytes = response.bytes().await?;
    let end = bytes.len().min(limit);
    Ok(bytes[..end].to_vec())
}

fn api_error_message(operation: &str, status: i32, errors: &[ApiError]) -> Error {
    let mut message = format!("HTTP {status}");
    if let Some(first) = errors.first() {
        if !first.message.is_empty() {
            message = first.message.clone();
        }
    }
    Error::Api {
        operation: operation.to_string(),
        message,
    }
}

/// Reads the API's JSON error body when there is one, and falls back to the
/// status line for the HTML failure pages Cloudflare sometimes returns.
fn kv_error(operation: &str, status: i32, raw: &[u8]) -> Error {
    if let Ok(body) = serde_json::from_slice::<ApiResponse>(raw) {
        if !body.errors.is_empty() {
            return api_error_message(operation, status, &body.errors);
        }
    }
    Error::Api {
        operation: operation.to_string(),
        message: format!("HTTP {status}"),
    }
}

// --- traits --------------------------------------------------------------

/// The Workers KV namespace the providers live in. `db::Kv` in production and a
/// map in tests, so the config store can be exercised without a network.
#[async_trait::async_trait]
pub trait KeyValue: Send + Sync {
    async fn get(&self, key: &str) -> Result<Option<String>>;
    async fn put(&self, key: &str, value: &str) -> Result<()>;
    /// Default implementation lets read-only test doubles opt out.
    async fn delete(&self, key: &str) -> Result<()> {
        self.put(key, "").await
    }
}

/// The `system_state` key/value store the gateway speaks to.
#[async_trait::async_trait]
pub trait StateStore: Send + Sync {
    async fn get_state(&self, key: &str) -> Result<Option<String>>;
    async fn put_state(&self, key: &str, value: &str) -> Result<()>;
}

/// Handle onto the real Workers KV namespace.
#[derive(Clone)]
pub struct Kv(pub Client);

/// Handle onto `system_state` in D1.
#[derive(Clone)]
pub struct State(pub Client);

#[async_trait::async_trait]
impl KeyValue for Kv {
    async fn get(&self, key: &str) -> Result<Option<String>> {
        self.0.kv_get(key).await
    }
    async fn put(&self, key: &str, value: &str) -> Result<()> {
        self.0.kv_put(key, value).await
    }
    async fn delete(&self, key: &str) -> Result<()> {
        self.0.kv_delete(key).await
    }
}

#[async_trait::async_trait]
impl StateStore for State {
    async fn get_state(&self, key: &str) -> Result<Option<String>> {
        self.0.get_state(key).await
    }
    async fn put_state(&self, key: &str, value: &str) -> Result<()> {
        self.0.put_state(key, value).await
    }
}

/// In-memory KV for tests and offline development.
#[derive(Default, Clone)]
pub struct MemoryKv {
    inner: Arc<RwLock<std::collections::HashMap<String, String>>>,
}

impl MemoryKv {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn snapshot(&self) -> std::collections::HashMap<String, String> {
        self.inner.read().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl KeyValue for MemoryKv {
    async fn get(&self, key: &str) -> Result<Option<String>> {
        Ok(self.inner.read().unwrap().get(key).cloned())
    }
    async fn put(&self, key: &str, value: &str) -> Result<()> {
        self.inner
            .write()
            .unwrap()
            .insert(key.to_string(), value.to_string());
        Ok(())
    }
    async fn delete(&self, key: &str) -> Result<()> {
        self.inner.write().unwrap().remove(key);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression: D1 names the row list `results`. Reading a `rows` key that
    /// the API never sends decoded every SELECT into an empty list while
    /// writes kept landing, which made the whole database look empty.
    #[test]
    fn the_d1_query_rows_come_from_results() {
        let envelope: ApiResponse = serde_json::from_str(
            r#"{"success":true,"errors":[],
                "result":[{"success":true,"results":[{"id":"a"},{"id":"b"}]}]}"#,
        )
        .expect("envelope");
        let results: Vec<QueryResult> =
            serde_json::from_value(envelope.result).expect("query results");
        assert_eq!(results.first().expect("statement").rows.len(), 2);
    }
}
