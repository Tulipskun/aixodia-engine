//! OpenCode Zen adapter: the free-tier request contract and the fallback ladder.
//!
//! Port of the engine's Go `provider/opencode` package.
//!
//! The contract, measured against the live endpoint:
//!
//! 1. The TLS ClientHello of the official client (a BoringSSL client, HTTP/1.1
//!    only, no GREASE): the free tier's gate reads the handshake.
//! 2. `Authorization: Bearer public`: the tier is anonymous, but a request with
//!    no `Authorization` is refused.
//! 3. `User-Agent: opencode/latest/<version>/cli`, the `x-opencode-*` headers,
//!    and a `ses_` id shared by the session/affinity headers.
//! 4. A system prompt ending in the client's `<env>` block, which is what the
//!    gate reads to tell the client apart from a generic API caller.
//! 5. Chat completions (`/chat/completions`) with the `shell` and `read` tool
//!    declarations, which is the shape the client sends.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use rand::RngCore;

use crate::openai;
use crate::types::{
    APIError, Adapter as AdapterTrait, Emitter, Message, Provider, Request, Response, ToolCall,
    ToolDef,
};

/// The name the client itself sends; read from its source and confirmed against
/// a recorded request.
pub const CLIENT_AGENT_MEASURED: &str = "opencode/latest/2.2.0/cli";

const SHELL_TOOL: &str = "shell";
const READ_TOOL: &str = "read";
const B62: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";

/// Supplies the real builtin tool declarations, so the contract injection uses
/// the same rich definitions the gateway offers instead of stubs. Wiring this to
/// `aixodia_tools::builtin` keeps the layering acyclic (tools depends on
/// provider, not the other way round).
pub type BuiltinFn = Arc<dyn Fn() -> HashMap<String, ToolDef> + Send + Sync>;

#[derive(Default)]
struct SessionState {
    session: String,
    session_at: Option<Instant>,
    agent: Option<String>,
}

/// The OpenCode adapter.
pub struct Adapter {
    inner: openai::Adapter,
    state: Arc<Mutex<SessionState>>,
    builtin: BuiltinFn,
}

impl Adapter {
    /// Uses placeholder `shell`/`read` declarations. Call
    /// [`with_builtin`](Self::with_builtin) with `aixodia_tools::builtin` for
    /// the real ones.
    pub fn new() -> Self {
        let state = Arc::new(Mutex::new(SessionState {
            session: new_id("ses_"),
            session_at: Some(Instant::now()),
            agent: None,
        }));
        let headers_state = state.clone();
        let inner = openai::Adapter::new()
            .with_header_func(Arc::new(move || {
                let (session, agent) = session_and_agent(&headers_state);
                headers(&session, agent.as_deref())
            }))
            .with_tls_client(fingerprint_client());
        Self {
            inner,
            state,
            builtin: Arc::new(HashMap::new),
        }
    }

    /// Injects the real builtin declarations for the contract.
    pub fn with_builtin(mut self, builtin: BuiltinFn) -> Self {
        self.builtin = builtin;
        self
    }

    /// Overrides the user agent.
    pub fn with_agent(self, agent: impl Into<String>) -> Self {
        self.state.lock().unwrap().agent = Some(agent.into());
        self
    }

    pub fn user_agent(&self) -> String {
        self.state
            .lock()
            .unwrap()
            .agent
            .clone()
            .unwrap_or_else(|| CLIENT_AGENT_MEASURED.to_string())
    }

    /// The id this engine calls itself in every header that carries one. It is
    /// replaced once an hour so a long-running engine never holds the same id it
    /// started with.
    pub fn session_id(&self) -> String {
        session_and_agent(&self.state).0
    }

    /// Answers a request, and knows one thing the ladder does not: a picture may
    /// be the reason the whole ladder failed.
    async fn stream_ladder(
        &self,
        req: &Request,
        prov: &Provider,
        key: &str,
        emit: &mut dyn Emitter,
    ) -> Result<Response> {
        let emitted = Arc::new(AtomicBool::new(false));
        let result = {
            let mut guard = |s: &str| -> Result<()> {
                emitted.store(true, Ordering::SeqCst);
                emit.emit(s)
            };
            self.ladder(req, prov, key, &mut guard).await
        };
        let resp = match result {
            Ok(resp) => return Ok(resp),
            Err(err) => err,
        };
        if emitted.load(Ordering::SeqCst)
            || !carries_images(&req.messages)
            || !worth_retrying_without_images(&resp)
        {
            return Err(resp);
        }
        let stripped = without_images(req);
        let mut guard = |s: &str| -> Result<()> {
            emitted.store(true, Ordering::SeqCst);
            emit.emit(s)
        };
        match self.ladder(&stripped, prov, key, &mut guard).await {
            Ok(retried) => Ok(retried),
            // Report the original failure: it is the one that describes what
            // was actually asked for.
            Err(_) => Err(resp),
        }
    }

    async fn ladder(
        &self,
        req: &Request,
        prov: &Provider,
        key: &str,
        guarded: &mut dyn Emitter,
    ) -> Result<Response> {
        let req = &with_contract_env(req);
        let emitted = Arc::new(AtomicBool::new(false));
        let note = |trace: &mut Vec<String>, name: &str, err: &anyhow::Error| {
            trace.push(format!("{name}: {}", describe_err(err)));
        };
        let fail = |trace: &[String], err: anyhow::Error| -> anyhow::Error {
            let attempts = trace.join(" | ");
            if let Some(api) = err.downcast_ref::<APIError>() {
                return APIError::new(
                    api.status_code,
                    format!("{} [attempts: {attempts}]", api.message),
                )
                .with_retry_after(api.retry_after)
                .into_anyhow();
            }
            if trace.is_empty() {
                err
            } else {
                anyhow!("{err} [attempts: {attempts}]")
            }
        };

        {
            let mut guard = |s: &str| -> Result<()> {
                emitted.store(true, Ordering::SeqCst);
                guarded.emit(s)
            };

            let mut trace: Vec<String> = Vec::new();

            // Attempt 1: chat completions with the contract tools, which is the
            // request shape the official client sends.
            let mut prepared = req.clone();
            prepared.tools = self.contract_tools(&req.tools);
            match self.inner.stream(&prepared, prov, key, &mut guard).await {
                Ok(resp) => return Ok(resp),
                Err(err) => {
                    note(&mut trace, "chat-stream-tools", &err);
                    if emitted.load(Ordering::SeqCst) || !is_tool_protocol_error(&err) {
                        return Err(fail(&trace, err));
                    }
                }
            }

            // Attempt 2: the Responses API with the contract tools.
            match self
                .inner
                .stream_via_responses(&prepared, prov, key, &mut guard)
                .await
            {
                Ok(resp) => return Ok(resp),
                Err(err) => {
                    note(&mut trace, "responses-stream-tools", &err);
                    if emitted.load(Ordering::SeqCst) || !is_tool_protocol_error(&err) {
                        return Err(fail(&trace, err));
                    }
                }
            }

            // Attempt 3: same streaming mode, no tools.
            let mut stripped = req.clone();
            stripped.tools = Vec::new();
            match self.inner.stream(&stripped, prov, key, &mut guard).await {
                Ok(resp) => return Ok(resp),
                Err(err) => {
                    note(&mut trace, "chat-stream-bare", &err);
                    if emitted.load(Ordering::SeqCst) || !is_tool_protocol_error(&err) {
                        return Err(fail(&trace, err));
                    }
                }
            }

            // Attempt 4: non-streaming, no tools.
            match self.inner.complete(&stripped, prov, key).await {
                Ok(bare) => {
                    guarded.emit(&bare.content)?;
                    return Ok(bare);
                }
                Err(err) => {
                    note(&mut trace, "chat-complete-bare", &err);
                    if !is_tool_protocol_error(&err) {
                        return Err(fail(&trace, err));
                    }
                }
            }

            // Attempt 5: Responses API, streaming, full tool set.
            match self
                .inner
                .stream_via_responses(&prepared, prov, key, &mut guard)
                .await
            {
                Ok(resp) => return Ok(resp),
                Err(err) => {
                    note(&mut trace, "responses-stream-tools", &err);
                    if emitted.load(Ordering::SeqCst) || !is_tool_protocol_error(&err) {
                        return Err(fail(&trace, err));
                    }
                }
            }

            // Attempt 6: Responses API, full tool set, non-streaming.
            match self
                .inner
                .complete_via_responses(&prepared, prov, key)
                .await
            {
                Ok(resp) => {
                    guarded.emit(&resp.content)?;
                    return Ok(resp);
                }
                Err(err) => {
                    note(&mut trace, "responses-complete-tools", &err);
                    if emitted.load(Ordering::SeqCst) || !is_tool_protocol_error(&err) {
                        return Err(fail(&trace, err));
                    }
                }
            }

            // Attempt 7: Responses API bare.
            match self
                .inner
                .complete_via_responses(&stripped, prov, key)
                .await
            {
                Ok(resp) => {
                    guarded.emit(&resp.content)?;
                    Ok(resp)
                }
                Err(err) => {
                    note(&mut trace, "responses-complete-bare", &err);
                    Err(fail(&trace, err))
                }
            }
        }
    }

    fn contract_tools(&self, given: &[ToolDef]) -> Vec<ToolDef> {
        let mut present: HashMap<&str, bool> = HashMap::new();
        for def in given {
            present.insert(def.name.as_str(), true);
        }
        let mut out = given.to_vec();
        let real = (self.builtin)();
        // The free-tier gate only passes the full rich declarations (it rejects
        // stub shells), so inject the real builtin defs — never minimal
        // sketches.
        for name in [SHELL_TOOL, READ_TOOL] {
            if !present.contains_key(name) {
                out.push(real.get(name).cloned().unwrap_or_else(|| placeholder(name)));
            }
        }
        out
    }
}

impl Default for Adapter {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl AdapterTrait for Adapter {
    fn name(&self) -> &str {
        "opencode"
    }

    async fn complete(&self, req: &Request, prov: &Provider, key: &str) -> Result<Response> {
        let mut content = String::new();
        let mut resp = self
            .stream(req, prov, key, &mut |delta: &str| {
                content.push_str(delta);
                Ok(())
            })
            .await?;
        // Prefer streamed deltas, but some paths (e.g. the Responses API
        // fallback resolved from a completed object) return full content
        // without emitting.
        if !content.is_empty() {
            resp.content = content;
        }
        Ok(resp)
    }

    async fn stream(
        &self,
        req: &Request,
        prov: &Provider,
        key: &str,
        emit: &mut dyn Emitter,
    ) -> Result<Response> {
        // The free tier is anonymous, but the client still authenticates with the
        // literal token `public`; a request with no Authorization is refused.
        let key = if key.is_empty() { "public" } else { key };
        self.stream_ladder(req, prov, key, emit).await
    }
}

fn session_and_agent(state: &Arc<Mutex<SessionState>>) -> (String, Option<String>) {
    let mut state = state.lock().unwrap();
    let stale = match state.session_at {
        Some(at) => at.elapsed() > Duration::from_secs(3600),
        None => true,
    };
    if stale {
        state.session = new_id("ses_");
        state.session_at = Some(Instant::now());
    }
    (state.session.clone(), state.agent.clone())
}

/// Everything the request-id headers would have carried is already in the
/// message and session headers, so the engine sends none of its own: the gate
/// reads an `x-opencode-request` header as a request that did not come from the
/// client and refuses it.
fn headers(session: &str, agent: Option<&str>) -> HashMap<String, String> {
    let agent = agent.unwrap_or(CLIENT_AGENT_MEASURED);
    HashMap::from([
        ("User-Agent".to_string(), agent.to_string()),
        ("x-opencode-client".to_string(), "cli".to_string()),
        ("x-opencode-project".to_string(), PROJECT_ID.to_string()),
        ("x-opencode-session".to_string(), session.to_string()),
        ("x-opencode-session-id".to_string(), session.to_string()),
        ("x-session-affinity".to_string(), session.to_string()),
        ("x-session-id".to_string(), session.to_string()),
    ])
}

/// A stable forty-hex project id, the width the client sends.
const PROJECT_ID: &str = "57d1d2efb78caaa2a5917f13969a24b57c24bbb9";

/// The environment block the official client ends its system prompt with. The
/// free tier reads it to tell the client apart from a generic API caller, so it
/// is part of the request contract rather than context for the model.
fn env_block() -> String {
    let dir = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "/root".to_string());
    let git = if std::path::Path::new(&dir).join(".git").exists() {
        "yes"
    } else {
        "no"
    };
    format!(
        "<env>\n  Working directory: {dir}\n  Workspace root folder: {dir}\n  Is directory a git repo: {git}\n  Platform: {}\n  Prefer /tmp/opencode over generic system temporary directories such as /tmp; it is pre-created and approved for external access.\n</env>",
        std::env::consts::OS
    )
}

/// The conversation with the contract's environment block in front. The client
/// puts it inside its own system prompt; a separate leading system turn keeps
/// the caller's instructions intact and is the same message shape on the wire.
fn with_contract_env(req: &Request) -> Request {
    if req
        .messages
        .iter()
        .any(|m| m.role == "system" && m.content.contains("<env>"))
    {
        return req.clone();
    }
    let mut out = req.clone();
    out.messages.insert(
        0,
        Message {
            role: "system".to_string(),
            content: env_block(),
            ..Message::default()
        },
    );
    out
}

/// Whether any message would put a picture on the wire.
fn carries_images(messages: &[Message]) -> bool {
    messages.iter().any(|m| !m.images.is_empty())
}

/// The same conversation with the pictures taken out and the last user message
/// told why, so the answer can be honest about what it saw.
fn without_images(req: &Request) -> Request {
    let mut out = req.clone();
    let mut last = None;
    for i in (0..out.messages.len()).rev() {
        if !out.messages[i].images.is_empty() {
            out.messages[i].images.clear();
            if last.is_none() && out.messages[i].role == "user" {
                last = Some(i);
            }
        }
    }
    if let Some(i) = last {
        let trimmed = out.messages[i].content.trim_end_matches('\n');
        out.messages[i].content = format!(
            "{trimmed}\n\n[the attached pictures were removed: this model does not accept images, so answer from the text alone and say that you could not see them]"
        );
    }
    out
}

/// Whether asking again without the pictures could plausibly succeed. The only
/// failures that genuinely cannot be helped by removing the pictures are the ones
/// about the key or the balance, so those are excluded and everything else is
/// retried once.
fn worth_retrying_without_images(err: &anyhow::Error) -> bool {
    let Some(api) = err.downcast_ref::<APIError>() else {
        return true;
    };
    match api.status_code {
        401 | 402 => false,
        403 => {
            // A 403 is also what a payload rejection looks like, so it is only
            // skipped when the message is plainly about credentials.
            let message = api.message.to_lowercase();
            !["key", "credit", "quota", "balance", "payment"]
                .iter()
                .any(|hint| message.contains(hint))
        }
        _ => true,
    }
}

/// Condenses an upstream error to `status: snippet` for traces.
fn describe_err(err: &anyhow::Error) -> String {
    let message = match err.downcast_ref::<APIError>() {
        Some(api) => format!("http{} {}", api.status_code, api.message),
        None => err.to_string(),
    };
    if message.chars().count() > 140 {
        let mut truncated: String = message.chars().take(140).collect();
        truncated.push('…');
        truncated
    } else {
        message
    }
}

/// Whether `err` looks like an upstream rejection of the function-calling
/// payload itself, in which case a tools-less retry is worth one attempt.
fn is_tool_protocol_error(err: &anyhow::Error) -> bool {
    let Some(api) = err.downcast_ref::<APIError>() else {
        return false;
    };
    let message = api.message.to_lowercase();
    // The free-tier gate rejects shapes without the full tool declarations,
    // usually as 403. A later attempt with the full tool set may still pass.
    if message.contains("free tier") && message.contains("opencode") {
        return true;
    }
    if api.status_code != 400 {
        return false;
    }
    if message.contains("protocol") {
        return true;
    }
    let has_tool_word = message.contains("tool") || message.contains("function");
    let has_reject_word = message.contains("support")
        || message.contains("invalid")
        || message.contains("not allowed")
        || message.contains("refuse");
    has_tool_word && has_reject_word
}

fn placeholder(name: &str) -> ToolDef {
    // The zen free-tier contract expects shell/read to be declared. They are
    // executed for real by the gateway, so describe them truthfully.
    let (description, properties, required) = if name == SHELL_TOOL {
        (
            "Executes a shell command on the host (sh -c) and returns combined stdout and stderr output.",
            serde_json::json!({
                "command": { "type": "string", "description": "Shell command to execute." }
            }),
            vec!["command"],
        )
    } else {
        (
            "Reads a text file from the host filesystem and returns its contents.",
            serde_json::json!({
                "path": { "type": "string", "description": "Absolute path of the file to read." }
            }),
            vec!["path"],
        )
    };
    let parameters = serde_json::json!({
        "type": "object",
        "properties": properties,
        "required": required,
    })
    .as_object()
    .cloned()
    .unwrap_or_default();
    ToolDef {
        name: name.to_string(),
        description: description.to_string(),
        parameters,
    }
}

// --- transport -----------------------------------------------------------

/// The cipher suites the official client offers, in its captured order. The
/// free tier's gate reads the ClientHello, so this list is part of the
/// contract rather than a preference.
const CIPHERS: &str = "TLS_AES_128_GCM_SHA256:TLS_AES_256_GCM_SHA384:TLS_CHACHA20_POLY1305_SHA256:ECDHE-ECDSA-AES128-GCM-SHA256:ECDHE-RSA-AES128-GCM-SHA256:ECDHE-ECDSA-AES256-GCM-SHA384:ECDHE-RSA-AES256-GCM-SHA384:ECDHE-ECDSA-CHACHA20-POLY1305:ECDHE-RSA-CHACHA20-POLY1305:ECDHE-ECDSA-AES128-SHA:ECDHE-RSA-AES128-SHA:ECDHE-ECDSA-AES256-SHA:ECDHE-RSA-AES256-SHA:AES128-GCM-SHA256:AES256-GCM-SHA384:AES128-SHA:AES256-SHA";

/// The captured group order, post-quantum hybrid first.
const CURVES: &str = "X25519MLKEM768:X25519:P-256:P-384";

/// The captured signature-algorithm order.
const SIGALGS: &str = "ecdsa_secp256r1_sha256:rsa_pss_rsae_sha256:rsa_pkcs1_sha256:ecdsa_secp384r1_sha384:rsa_pss_rsae_sha384:rsa_pkcs1_sha384:rsa_pss_rsae_sha512:rsa_pkcs1_sha512:ed25519:ecdsa_secp521r1_sha512:rsa_pkcs1_sha1";

/// A BoringSSL client configured to present the official client's hello.
///
/// The endpoint serves its free tier only to requests whose TLS handshake
/// matches the CLI's. The CLI is a BoringSSL client: plain BoringSSL extension
/// order, no GREASE, HTTP/1.1 only, post-quantum X25519MLKEM768 first. These
/// settings reproduce that hello.
fn fingerprint_client() -> wreq::Client {
    static CLIENT: OnceLock<wreq::Client> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            let tls = wreq::tls::TlsOptions::builder()
                .alpn_protocols([wreq::tls::AlpnProtocol::HTTP1])
                .min_tls_version(wreq::tls::TlsVersion::TLS_1_2)
                .max_tls_version(wreq::tls::TlsVersion::TLS_1_3)
                .grease_enabled(false)
                .session_ticket(true)
                .renegotiation(true)
                .enable_ocsp_stapling(true)
                .enable_signed_cert_timestamps(true)
                .cipher_list(CIPHERS)
                .preserve_tls13_cipher_list(true)
                .curves_list(CURVES)
                .sigalgs_list(SIGALGS)
                .build();
            wreq::Client::builder()
                .tls_options(tls)
                .build()
                .expect("opencode TLS client")
        })
        .clone()
}

// --- id generation -------------------------------------------------------

static ID_STATE: Mutex<(i64, i64)> = Mutex::new((0, 0));

/// The six-byte head the client puts on every id it hands out: the current
/// milliseconds shifted up twelve bits to leave room for a per-millisecond
/// counter, added, inverted so that ids sort newest-first as text, and kept to
/// six bytes.
///
/// The free tier reads two things out of the id: the low twelve bits (the
/// counter, which must be below 64) and the tail (fourteen letters/digits). The
/// timestamp in between is not consulted.
fn stamp() -> i64 {
    let millis = now_millis();
    let mut state = ID_STATE.lock().unwrap();
    if millis != state.0 {
        state.0 = millis;
        state.1 = 0;
    }
    state.1 += 1;
    !((millis << 12).wrapping_add(state.1))
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn new_id(prefix: &str) -> String {
    let encoded = stamp();
    let mut raw = [0u8; 6];
    for (i, byte) in raw.iter_mut().enumerate() {
        *byte = (encoded >> (40 - 8 * i)) as u8;
    }

    let mut noise = [0u8; 14];
    if rand::thread_rng().try_fill_bytes(&mut noise).is_err() {
        for (i, byte) in noise.iter_mut().enumerate() {
            *byte = (encoded >> ((i % 8) * 8)) as u8;
        }
    }
    let suffix: String = noise
        .iter()
        .map(|byte| B62[*byte as usize % B62.len()] as char)
        .collect();
    format!("{prefix}{}{}", hex::encode(raw), suffix)
}

/// Re-exported for the gateway's tool-call trail.
pub fn tool_call(id: &str, name: &str, arguments: &str) -> ToolCall {
    ToolCall {
        id: id.to_string(),
        name: name.to_string(),
        arguments: arguments.to_string(),
    }
}
