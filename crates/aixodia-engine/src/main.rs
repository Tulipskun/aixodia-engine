//! aixodia-engine: the engine binary.
//!
//! Port of the Go `main` package: verify the token, open D1, build the gateway,
//! the run manager, the phone hub and the terminal, serve them on
//! `127.0.0.1:8787`, put a Cloudflare Named Tunnel in front, and publish its
//! stable endpoint through Workers KV.

#[allow(dead_code)]
mod auth_exchange;
mod client_auth;
mod d1log;
mod devices;
mod handover;
mod tunnel;

use std::future::IntoFuture;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::State;
use axum::middleware;
use axum::routing::get;
use futures_util::StreamExt;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use aixodia_config as config;
use aixodia_db::Client as DbClient;
use aixodia_io::gateway::SessionStore;
use aixodia_io::Gateway;
use aixodia_mobile::{ChatCall, ChatResult, Hub};
use aixodia_run::{ChatExecutor, ChatJob, D1Store, Manager, SseSink};
use aixodia_selfupdate as selfupdate;
use aixodia_session::D1;
use aixodia_terminal::Server as TerminalServer;
use aixodia_tools::run::tools_with;

use handover::UnfinishedStore;
use tunnel::{now_unix, version_string};

/// Counts the turns the phone hub is running. A deploy waits for zero.
static ACTIVE_CHATS: AtomicI64 = AtomicI64::new(0);

fn main() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    runtime.block_on(async_main());
}

async fn async_main() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();

    tunnel::set_started_at(now_unix());

    if std::env::args().nth(1).as_deref() == Some("update") {
        std::process::exit(update_once().await);
    }

    let cancel = CancellationToken::new();
    let signal_cancel = cancel.clone();
    tokio::spawn(async move {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = terminate.recv() => {}
        }
        signal_cancel.cancel();
    });

    if let Err(err) = run(cancel.clone()).await {
        d1log::error(err.to_string());
        eprintln!("aixodia-engine: {err}");
        std::process::exit(1);
    }

    cancel.cancelled().await;
    d1log::info("engine stopped");
}

async fn run(cancel: CancellationToken) -> anyhow::Result<()> {
    let token = std::env::var("CF_TOKEN")
        .unwrap_or_default()
        .trim()
        .to_string();
    if token.is_empty() {
        anyhow::bail!("CF_TOKEN is not set");
    }
    let db = DbClient::new();
    if let Err(reason) = db.verify_detailed(&token).await {
        // The reason matters: a bad token and an unreachable API both used to
        // print "CF_TOKEN is invalid", which sent operators to the wrong place.
        anyhow::bail!("CF_TOKEN could not be verified: {reason}");
    }
    // GitHub bearer verification for the relay API surface. The allowlist comes
    // from ALLOWED_GITHUB_USERS; unset or empty fails closed — every bearer is
    // rejected and the static-token comparison is gone.
    let github_auth = std::sync::Arc::new(aixodia_io::GitHubAuth::new(
        &std::env::var("ALLOWED_GITHUB_USERS").unwrap_or_default(),
    ));
    if let Err(err) = db.verify_kv().await {
        tracing::warn!("provider kv: {err}");
    }
    db.verify_config_kv().await?;

    // Browser clients no longer go through the dashboard Worker: the engine
    // issues its own short-lived token and answers CORS preflights itself.
    // ALLOWED_* fall back to Workers KV AIXODIA_CONFIG, so verify the db first.
    let client_auth = client_auth::ClientAuth::from_env(token.clone(), &db).await;
    let tunnel_token = db
        .config_kv_get("tunnel_token")
        .await?
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("Workers KV key tunnel_token is missing or empty"))?;
    let tunnel_url = db
        .config_kv_get("tunnel_url")
        .await?
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("Workers KV key tunnel_url is missing or empty"))?;
    let parsed_tunnel_url = url::Url::parse(tunnel_url.trim())
        .map_err(|_| anyhow::anyhow!("Workers KV key tunnel_url is not a valid URL"))?;
    if parsed_tunnel_url.scheme() != "https" || parsed_tunnel_url.host_str().is_none() {
        anyhow::bail!("Workers KV key tunnel_url must be an https:// URL");
    }

    d1log::start()?;
    d1log::info("engine started");
    tracing::info!("engine started");

    let store = Arc::new(D1::new(db.clone()));
    store.ensure_turns().await;
    let objectives = Arc::new(aixodia_goals::D1::new(db.clone()));
    objectives.ensure().await;
    config::set_store(Some(Arc::new(config::Store::new(Arc::new(db.kv())))));

    spawn_config_reloader(cancel.clone(), db.clone());

    let registry = Arc::new(aixodia_provider::Registry::new());
    let tools = tools_with(Some(objectives.clone() as Arc<dyn aixodia_goals::Store>));
    let gateway = Arc::new(Gateway::new(
        store.clone() as Arc<dyn SessionStore>,
        registry,
        tools,
    ));
    gateway.set_objective_store(objectives.clone() as Arc<dyn aixodia_goals::Store>);
    gateway.set_state_store(Arc::new(db.state()));
    gateway.set_started_at(tunnel::started_at());
    gateway.set_log_reader(Arc::new(|limit: i64, level: &str| {
        d1log::recent(limit, level)
            .into_iter()
            .map(Value::Object)
            .collect()
    }));
    gateway.set_upload_dir(upload_dir());

    let executor = Arc::new(GatewayExecutor {
        gateway: gateway.clone(),
    });
    let runs = Arc::new(Manager::new(Arc::new(D1Store::new(db.clone())), executor));
    runs.start().await?;

    let hub = Arc::new(Hub::new());
    let chat = chat_func(gateway.clone());
    let mut terminal_server = TerminalServer::new(token.clone());
    terminal_server.tmux =
        aixodia_terminal::tmux_available() && std::env::var("AIXODIA_TMUX").as_deref() != Ok("0");
    if terminal_server.tmux {
        aixodia_terminal::tmux::hide_status_bars();
        aixodia_terminal::tmux::disable_mouse();
    }
    let terminal = Arc::new(terminal_server);
    terminal.set_logf(Arc::new(|message| {
        d1log::info(message.clone());
        tracing::info!("{message}");
    }));

    // The relay dispatches rpc frames through this in-process mux, exactly as
    // the phone's direct HTTP calls reach the same routes.
    let rpc = gateway
        .router()
        .merge(aixodia_run::router(runs.clone()))
        .merge(terminal.router());
    let relay_cfg = Arc::new(aixodia_relay::Config {
        token: token.clone(),
        api_token: token.clone(),
        hub: hub.clone(),
        chat: chat.clone(),
        api: rpc.layer(middleware::from_fn_with_state(
            github_auth.clone(),
            aixodia_io::require_token,
        )),
        term: terminal.clone(),
    });

    let hub_for_ws = hub.clone();
    let token_for_ws = token.clone();
    let chat_for_ws = chat.clone();
    let mut mux = gateway
        .router()
        .merge(aixodia_run::router(runs.clone()))
        .merge(terminal.router())
        .merge(aixodia_relay::router(relay_cfg))
        .merge(client_auth.router())
        .merge(devices::Devices::new(db.clone()).router())
        .route(
            "/ws",
            get(move |request: axum::extract::Request| {
                let hub = hub_for_ws.clone();
                let token = token_for_ws.clone();
                let chat = chat_for_ws.clone();
                async move { aixodia_mobile::handler(request, hub, token, chat) }
            }),
        );
    mux = mux.layer(middleware::from_fn_with_state(
        client_auth.clone(),
        client_auth::client_guard,
    ));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:8787").await?;
    tracing::info!("listening on 127.0.0.1:8787");

    // Publish the stable Named Tunnel URL only after cloudflared has registered
    // an edge connection. The hostname and origin route are configured in Cloudflare.
    let cloudflared =
        which("cloudflared").ok_or_else(|| anyhow::anyhow!("cloudflared is not installed"))?;
    let publisher_db = db.clone();
    let publisher_hub = hub.clone();
    let noted = Arc::new(AtomicI64::new(0));
    let keeper = Arc::new(tunnel::Keeper::new(
        cloudflared,
        tunnel_token,
        tunnel_url,
        Some(Arc::new(move |url: String| {
            let db = publisher_db.clone();
            let hub = publisher_hub.clone();
            let noted = noted.clone();
            tokio::spawn(async move {
                let version = version_string();
                match db.config_kv_put_if_changed("daemon/url", &url).await {
                    Ok(true) => tracing::info!("daemon url updated in Workers KV"),
                    Ok(false) => tracing::debug!("daemon url unchanged in Workers KV"),
                    Err(err) => {
                        tracing::warn!("publish daemon url: {err}");
                        return;
                    }
                }
                if let Err(err) = db
                    .config_kv_put_if_changed("daemon/version", &version)
                    .await
                {
                    tracing::warn!("publish daemon version: {err}");
                    return;
                }
                let _ = hub
                    .broadcast_daemon(&url, &version, tunnel::started_at(), true)
                    .await;
                if noted.swap(1, Ordering::SeqCst) == 0 {
                    handover::publish_handover(&url).await;
                }
            });
        })),
        home_dir().join(tunnel::PIDFILE),
    ));
    handover::set_session_store(Some(store.clone() as Arc<dyn UnfinishedStore>));
    handover::set_put_handover_state(Some(Arc::new(db.state())));
    tokio::spawn({
        let keeper = keeper.clone();
        let cancel = cancel.clone();
        async move { keeper.run(cancel).await }
    });

    aixodia_io::auto_backup::spawn(gateway.clone(), cancel.clone());

    let idle = {
        let runs = runs.clone();
        let gateway = gateway.clone();
        move || {
            ACTIVE_CHATS.load(Ordering::SeqCst) == 0
                && runs.active() == 0
                // A turn that answered before its rows landed still owes D1 the
                // write, and an in-place update cannot finish it afterwards.
                && !gateway.owes_backups()
        }
    };
    // Updates install only while no chat is in flight, and always as an
    // in-place exec, so the pid never changes. A request that arrives during a
    // chat is queued and applied by the loop once the engine goes idle.
    let update_idle = idle.clone();
    let update_cancel = cancel.clone();
    tokio::spawn(async move {
        selfupdate::run_loop(update_cancel, update_idle).await;
    });
    let _update = selfupdate::notify_update(idle);

    let shutdown = cancel.clone();
    let server = axum::serve(listener, mux.into_make_service())
        .with_graceful_shutdown(async move { shutdown.cancelled().await })
        .into_future();
    tokio::pin!(server);
    tokio::select! {
        result = &mut server => {
            result?;
        }
        _ = cancel.cancelled() => {
            // Stop the turns this process owns before waiting for connections,
            // and give a wedged SSE stream the same five seconds Go does.
            runs.close();
            let _ = tokio::time::timeout(Duration::from_secs(5), &mut server).await;
        }
    }
    // A turn can answer while its rows are still on their way to D1, and those
    // writes belong to this process: an exit must not take the catch-up with it.
    let _ = tokio::time::timeout(Duration::from_secs(5), gateway.await_all_backups()).await;
    Ok(())
}

/// One chat turn through the gateway's HTTP chat route, in process, exactly as
/// Go's `chatViaGateway` does. The phone socket and the API share routing,
/// history and persistence.
fn chat_func(gateway: Arc<Gateway>) -> aixodia_mobile::ChatFunc {
    Arc::new(move |call: ChatCall| {
        let gateway = gateway.clone();
        Box::pin(async move {
            ACTIVE_CHATS.fetch_add(1, Ordering::SeqCst);
            let result = chat_once(gateway, call).await;
            ACTIVE_CHATS.fetch_sub(1, Ordering::SeqCst);
            result
        })
    })
}

async fn chat_once(gateway: Arc<Gateway>, call: ChatCall) -> Result<ChatResult, String> {
    let attachments: Vec<Value> = call
        .attachments
        .iter()
        .map(|a| json!({"path": a.path, "name": a.name, "mime": a.mime}))
        .collect();
    let messages = if call.retry {
        Vec::new()
    } else {
        vec![json!({"role": "user", "content": call.text})]
    };
    let body = serde_json::to_vec(&json!({
        "session_id": call.session_id,
        "stream": true,
        "append": true,
        "retry": call.retry,
        "messages": messages,
        "attachments": attachments,
    }))
    .map_err(|err| err.to_string())?;

    let response = aixodia_io::chat::chat(State(gateway.clone()), Bytes::from(body)).await;

    let mut result = ChatResult::default();
    if response.status() != axum::http::StatusCode::OK {
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .map_err(|err| err.to_string())?;
        return Err(format!(
            "chat {}: {}",
            status.as_u16(),
            String::from_utf8_lossy(&bytes).trim()
        ));
    }

    let mut pending: Vec<u8> = Vec::new();
    let mut full = String::new();
    let mut in_tokens = 0i64;
    let mut out_tokens = 0i64;
    let mut stream_error = String::new();
    let mut stream = response.into_body().into_data_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|err| err.to_string())?;
        pending.extend_from_slice(&chunk);
        while let Some(index) = find(&pending, b"\n\n") {
            let event: Vec<u8> = pending.drain(..index + 2).collect();
            let Some(data) = event.strip_prefix(b"data: ") else {
                continue;
            };
            let data = data.strip_suffix(b"\n\n").unwrap_or(data);
            if data == b"[DONE]" {
                continue;
            }
            let Ok(value) = serde_json::from_slice::<Value>(data) else {
                continue;
            };
            if let Some(turns) = value.get("turns").and_then(Value::as_array) {
                for turn in turns {
                    if let Ok(turn) = serde_json::from_value(turn.clone()) {
                        result.turns.push(turn);
                    }
                }
            }
            if let Some(turn) = value.get("turn") {
                if let Ok(turn) = serde_json::from_value(turn.clone()) {
                    result.answer = turn;
                }
                result.turns_synced = value
                    .get("turns_synced")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
            }
            if let Some(error) = value
                .get("error")
                .and_then(|error| error.get("message"))
                .and_then(Value::as_str)
            {
                stream_error = error.to_string();
                continue;
            }
            if let Some(usage) = value.get("usage") {
                in_tokens = usage
                    .get("prompt_tokens")
                    .and_then(Value::as_i64)
                    .unwrap_or(in_tokens);
                out_tokens = usage
                    .get("completion_tokens")
                    .and_then(Value::as_i64)
                    .unwrap_or(out_tokens);
            }
            if let Some(choices) = value.get("choices").and_then(Value::as_array) {
                for choice in choices {
                    if let Some(text) = choice
                        .get("delta")
                        .and_then(|delta| delta.get("content"))
                        .and_then(Value::as_str)
                    {
                        if !text.is_empty() {
                            full.push_str(text);
                            (call.on_delta)(text);
                        }
                    }
                }
            }
            if let Some(tool) = value.get("tool_event").and_then(Value::as_object) {
                let event = aixodia_mobile::ToolEvent {
                    stage: string_field(tool, "stage"),
                    name: string_field(tool, "name"),
                    call_id: string_field(tool, "call_id"),
                    arguments: string_field(tool, "arguments"),
                    text: string_field(tool, "text"),
                    is_error: tool
                        .get("is_error")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    duration_ms: tool.get("duration_ms").and_then(Value::as_i64).unwrap_or(0),
                    round: tool.get("round").and_then(Value::as_i64).unwrap_or(0),
                    state: string_field(tool, "state"),
                    prompt_tokens: tool
                        .get("prompt_tokens")
                        .and_then(Value::as_i64)
                        .unwrap_or(0),
                    completion_tokens: tool
                        .get("completion_tokens")
                        .and_then(Value::as_i64)
                        .unwrap_or(0),
                    cache_read_tokens: tool
                        .get("cache_read_tokens")
                        .and_then(Value::as_i64)
                        .unwrap_or(0),
                    cache_write_tokens: tool
                        .get("cache_write_tokens")
                        .and_then(Value::as_i64)
                        .unwrap_or(0),
                    reasoning_tokens: tool
                        .get("reasoning_tokens")
                        .and_then(Value::as_i64)
                        .unwrap_or(0),
                    input_includes_cache: tool
                        .get("input_includes_cache")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                };
                (call.on_tool)(event);
            }
        }
    }
    if !stream_error.is_empty() {
        return Err(format!("chat: {stream_error}"));
    }
    if result.answer.text.is_empty() && result.answer.seq == 0 {
        result.answer.text = full;
        result.answer.input_tokens = in_tokens;
        result.answer.output_tokens = out_tokens;
    }
    Ok(result)
}

fn string_field(map: &serde_json::Map<String, Value>, key: &str) -> String {
    map.get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// The gateway's executor for a run: it writes the same SSE stream into the
/// run's sink, so the run API and a direct client see one implementation.
struct GatewayExecutor {
    gateway: Arc<Gateway>,
}

#[async_trait::async_trait]
impl ChatExecutor for GatewayExecutor {
    async fn execute(
        &self,
        job: ChatJob,
        sink: Arc<SseSink>,
        cancel: CancellationToken,
    ) -> Result<(), String> {
        let messages = if job.retry {
            Vec::new()
        } else {
            vec![json!({"role": "user", "content": job.message})]
        };
        let attachments: Vec<Value> = job
            .attachments
            .iter()
            .map(|a| json!({"path": a.path, "name": a.name, "mime": a.mime}))
            .collect();
        let body = serde_json::to_vec(&json!({
            "session_id": job.session_id,
            "stream": true,
            "append": true,
            "retry": job.retry,
            "messages": messages,
            "attachments": attachments,
        }))
        .map_err(|err| err.to_string())?;
        let response = tokio::select! {
            response = aixodia_io::chat::chat(
                State(self.gateway.clone()),
                Bytes::from(body),
            ) => response,
            _ = cancel.cancelled() => return Err("cancelled".to_string()),
        };
        sink.write_header(response.status().as_u16());
        let mut stream = response.into_body().into_data_stream();
        loop {
            let next = tokio::select! {
                chunk = stream.next() => chunk,
                _ = cancel.cancelled() => return Err("cancelled".to_string()),
            };
            match next {
                Some(Ok(chunk)) => sink.write(&chunk),
                Some(Err(err)) => return Err(err.to_string()),
                None => return Ok(()),
            }
        }
    }
}

async fn update_once() -> i32 {
    match selfupdate::need_update().await {
        Ok((false, _)) => {
            println!("already the published build");
            0
        }
        Ok((true, release)) => {
            match selfupdate::install(&release, &selfupdate::target_path()).await {
                Ok(false) => {
                    println!("already the published build");
                    0
                }
                Ok(true) => {
                    println!("installed the build published {}", release.updated_at);
                    0
                }
                Err(err) => {
                    eprintln!("selfupdate: {err}");
                    1
                }
            }
        }
        Err(err) => {
            eprintln!("selfupdate: {err}");
            1
        }
    }
}

fn spawn_config_reloader(cancel: CancellationToken, db: DbClient) {
    tokio::spawn(async move {
        let load = |db: DbClient| {
            let db = db.clone();
            async move {
                if !db.kv_ready() {
                    if let Err(err) = db.verify_kv().await {
                        tracing::warn!("kv: {err}");
                        return;
                    }
                }
                if let Err(err) = aixodia_config::load_config().await {
                    tracing::warn!("config: {err}");
                }
            }
        };
        load(db.clone()).await;
        let mut ticker = tokio::time::interval(Duration::from_secs(60));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = ticker.tick() => load(db.clone()).await,
            }
        }
    });
}

/// The engine's local state directory. The keeper records only its child
/// cloudflared pid here so an in-place executable update can reap an old tunnel.
fn home_dir() -> std::path::PathBuf {
    match std::env::var_os("AIXODIA_HOME").filter(|home| !home.is_empty()) {
        Some(home) => std::path::PathBuf::from(home),
        None => match std::env::var_os("HOME").filter(|home| !home.is_empty()) {
            Some(home) => std::path::PathBuf::from(home).join(".aixodia"),
            None => std::path::PathBuf::from("."),
        },
    }
}

fn upload_dir() -> String {
    home_dir().join("uploads").to_string_lossy().into_owned()
}

fn which(name: &str) -> Option<std::path::PathBuf> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}
