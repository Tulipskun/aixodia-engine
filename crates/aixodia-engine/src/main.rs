//! aixodia-engine: the engine binary.
//!
//! Port of the Go `main` package: verify the token, open D1, build the gateway,
//! the run manager, the phone hub and the terminal, serve them on
//! `127.0.0.1:8787`, put a Cloudflare Named Tunnel in front, and publish its
//! stable endpoint through Workers KV.

#[allow(dead_code)]
mod auth_exchange;
mod auth_gate;
mod client_auth;
mod d1log;
mod devices;
mod handover;
mod tunnel;

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::middleware;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use futures_util::StreamExt;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;
use tower::ServiceExt as _;

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
    // Log verbosity is a built-in default, not an environment configuration.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new("info"))
        .try_init();

    tunnel::set_started_at(now_unix());

    if std::env::args().nth(1).as_deref() == Some("update") {
        std::process::exit(update_once().await);
    }

    // The shell holder: the engine starts this binary again to own a terminal's
    // pty, so the shell is not a child of the engine's lifetime and survives it.
    if std::env::args().nth(1).as_deref() == Some("term-holder") {
        let arg = |n: usize| std::env::args().nth(n);
        let id = arg(2).unwrap_or_default();
        let shell = arg(3).unwrap_or_else(|| "/bin/bash".to_string());
        let cols: u16 = arg(4).and_then(|v| v.parse().ok()).unwrap_or(80);
        let rows: u16 = arg(5).and_then(|v| v.parse().ok()).unwrap_or(24);
        aixodia_terminal::run_holder(&id, &shell, cols, rows, None);
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

    // The socket comes first, before anything that talks to the network.
    //
    // An update replaces this image with `execve`, and the image it replaces
    // held the listening socket. Taking the port over is therefore the very
    // first thing the new image has to do: every check and read below crosses
    // the tunnel and costs seconds, and doing them first left the port
    // unaccepted for as long as they took — which the phone saw as the engine
    // being gone, even though the swap itself had no gap at all.
    let listener = listen();

    // Start answering on the inherited socket before reading any configuration.
    //
    // The reads below cross the tunnel and take tens of seconds together. Until
    // this process calls `accept`, a request that arrived during the `execve`
    // sits in the accept queue unanswered, and the phone reads that silence as
    // the engine being gone. So the routes are served through a slot that holds
    // the real router once it is built: `/healthz` answers from the first
    // instant (that is the request the tunnel watches), anything else is told
    // plainly that the engine is still starting rather than left to time out.
    let routes: SharedRoutes = Arc::new(tokio::sync::RwLock::new(None));
    let booting = routes.clone();
    let boot = Router::new().fallback(move |request: axum::extract::Request| {
        let booting = booting.clone();
        async move {
            if let Some(router) = booting.read().await.clone() {
                return router
                    .oneshot(request)
                    .await
                    .unwrap_or_else(|never| match never {});
            }
            if request.uri().path() == "/healthz" {
                let body = json!({
                    "ok": true,
                    "version": aixodia_io::gateway::VERSION,
                    "pid": std::process::id(),
                    "update": "signal:USR1",
                    "starting": true,
                });
                return (StatusCode::OK, axum::Json(body)).into_response();
            }
            (StatusCode::SERVICE_UNAVAILABLE, "engine is starting").into_response()
        }
    });
    let shutting_down = cancel.clone();
    let mut serving_task = tokio::spawn(async move {
        axum::serve(
            listener,
            boot.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .with_graceful_shutdown(async move { shutting_down.cancelled().await })
        .await
    });

    if let Err(reason) = db.verify_detailed(&token).await {
        // The reason matters: a bad token and an unreachable API both used to
        // print "CF_TOKEN is invalid", which sent operators to the wrong place.
        anyhow::bail!("CF_TOKEN could not be verified: {reason}");
    }
    if let Err(err) = db.verify_kv().await {
        tracing::warn!("provider kv: {err}");
    }
    db.verify_config_kv().await?;

    // GitHub bearer allowlist is operator configuration in AIXODIA_CONFIG.
    // Missing/empty fails closed; KV read errors abort startup rather than silently
    // turning a configuration failure into an authorization failure.
    let github_users = db
        .config_kv_get("client/allowed_users")
        .await?
        .unwrap_or_default();
    let github_auth = std::sync::Arc::new(aixodia_io::GitHubAuth::new(&github_users));

    // Browser clients no longer go through the dashboard Worker: the engine
    // issues its own short-lived token and answers CORS preflights itself.
    // ALLOWED_* fall back to Workers KV AIXODIA_CONFIG, so verify the db first.
    let client_auth = client_auth::ClientAuth::from_kv(token.clone(), &db).await?;
    // Pulls the Worker's live token set before answering requests, then keeps it
    // current every minute so a token issued while this process was down still
    // works (see auth_system.md's sync rules).
    client_auth.spawn_sync_task();
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
    let terminal_server = TerminalServer::new(token.clone());
    // The screen is a hub per process, like the terminal server: one display,
    // one capture, however many phones are looking at it.
    let display = db
        .config_kv_get("runtime/display")
        .await?
        .unwrap_or_default();
    let screen = aixodia_screen::ScreenHub::new(display, token.clone());
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
        .merge(aixodia_screen::router(screen.clone()))
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
        .merge(aixodia_screen::router(screen.clone()))
        .merge(terminal.router())
        .merge(aixodia_relay::router(relay_cfg))
        .merge(client_auth.router())
        .merge(devices::Devices::new(db.clone()).await?.router())
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

    // What "safe to replace this image" means: no turn is being answered and no
    // run is being driven. Both are work whose result is owed to someone still
    // connected, and an exec ends it mid-answer.
    //
    // A write queued to D1 deliberately does not count. Waiting for one was
    // wrong twice over: `execve` ends the runtime whether or not the queue is
    // empty, so waiting only moves the loss later instead of preventing it, and
    // a D1 call that hangs left the condition false forever with nothing else
    // running — an engine that could then never be updated at all. The queue is
    // drained on shutdown, where the engine can afford to wait, and a queued
    // write lost to an update is the history of a turn whose answer the caller
    // already has.
    let idle = {
        let runs = runs.clone();
        move || ACTIVE_CHATS.load(Ordering::SeqCst) == 0 && runs.active() == 0
    };
    // Updates install only while no chat is in flight, and always as an
    // in-place exec, so the pid never changes. Nothing polls the release page:
    // an install happens when `aixodia update` signals this process, and not
    // otherwise. A scheduled check replaced the running binary underneath
    // whoever was connected — a terminal asking for a ticket during the swap
    // got no answer and the screen read that as the engine being gone.
    let update_idle = idle.clone();
    let _update = selfupdate::notify_update(idle);
    let _update_loop = tokio::spawn(async move {
        // A request queued while a chat was in flight has to get its moment:
        // nothing else calls `apply_pending` except the end of a chat, and a
        // machine whose chat has been quiet for hours would wait forever. This
        // loop does not poll the release page — it only checks in when a queue
        // exists, so an idle install is still driven by `aixodia update` and
        // never by the clock.
        loop {
            tokio::time::sleep(Duration::from_secs(30)).await;
            if selfupdate::is_pending() && update_idle() {
                selfupdate::apply_pending().await;
            }
        }
    });

    // Every route is up; the requests that were waiting in the accept queue are
    // answered by the real router from this point on.
    *routes.write().await = Some(mux);
    tracing::info!("routes installed");

    tokio::select! {
        result = &mut serving_task => {
            match result {
                Ok(served) => served?,
                Err(err) => return Err(err.into()),
            }
        }
        _ = cancel.cancelled() => {
            // Stop the turns this process owns before waiting for connections,
            // and give a wedged SSE stream the same five seconds Go does.
            runs.close();
            let _ = tokio::time::timeout(Duration::from_secs(5), &mut serving_task).await;
        }
    }
    // A turn can answer while its rows are still on their way to D1, and those
    // writes belong to this process: an exit must not take the catch-up with it.
    let _ = tokio::time::timeout(Duration::from_secs(5), gateway.await_all_backups()).await;
    // The display and the window manager the screen feature brought up belong to
    // this process. Without this the Xvfb outlives the engine, keeps its socket
    // on the display number, and the next start adopts a server whose window
    // manager is gone — a picture with no way to move a window. It also stacks a
    // server per restart.
    aixodia_screen::display::shutdown();
    Ok(())
}

/// One chat turn through the gateway's HTTP chat route, in process, exactly as
/// Go's `chatViaGateway` does. The phone socket and the API share routing,
/// history and persistence.
/// The router the listening socket dispatches to, empty until it is built.
///
/// The socket has to accept before the engine has read its configuration, so the
/// accept loop is started with a stand-in and given the real router through this
/// slot once every route exists.
type SharedRoutes = Arc<tokio::sync::RwLock<Option<Router>>>;

/// Takes the port this process must answer on, as early as possible.
///
/// An inherited socket is adopted as-is; otherwise the port is bound here. Called
/// before the configuration reads so an update's new image is accept-ready
/// immediately rather than after the network round-trips those reads need.
fn listen() -> tokio::net::TcpListener {
    let listener = match inherited_listener() {
        Some(listener) => {
            tracing::info!("listening on 127.0.0.1:8787 (inherited from the previous image)");
            listener
        }
        None => {
            let bound = std::net::TcpListener::bind("127.0.0.1:8787")
                .expect("the engine must own 127.0.0.1:8787");
            bound
                .set_nonblocking(true)
                .expect("a non-blocking listener");
            tracing::info!("listening on 127.0.0.1:8787");
            tokio::net::TcpListener::from_std(bound).expect("a tokio listener")
        }
    };
    // An update carries this socket across the swap, so the port is never
    // unbound: requests arriving during the `execve` wait in the accept queue
    // instead of being refused.
    {
        use std::os::unix::io::AsRawFd as _;
        selfupdate::keep_listener_across_update(listener.as_raw_fd());
    }
    listener
}

/// The env var naming the listener an update handed over.
const HANDOVER_LISTENER_ENV: &str = "AIXODIA_HANDOVER_LISTENER_FD";

/// Takes the listening socket this process inherited, if any.
///
/// An update replaces this process with `execve`, which closes every descriptor
/// not marked to survive it. The socket therefore has to be both marked and
/// described to the next image, and it is the *same* kernel socket either way:
/// connections queued while the swap happened are still there to accept, so no
/// request is refused.
fn inherited_listener() -> Option<tokio::net::TcpListener> {
    use std::os::unix::io::FromRawFd as _;

    let raw: std::os::unix::io::RawFd = std::env::var(HANDOVER_LISTENER_ENV).ok()?.parse().ok()?;
    // The variable must not leak into anything this image starts.
    std::env::remove_var(HANDOVER_LISTENER_ENV);
    if raw < 0 || !is_listening_socket(raw) {
        // A name that does not describe a listening socket is an inheritance
        // that did not happen — a launcher that copied the environment, or a
        // shell that exported it. Adopting the number anyway would take over
        // whatever descriptor now holds it, and a closed one would make the
        // bind below fail with "address already in use" for a port nothing
        // listens on.
        return None;
    }
    // The descriptor was made inheritable so it could cross the `execve` that
    // replaced the previous image, and that flag is still on it. Every process
    // this one starts would inherit a listening socket it has no business
    // holding — a shell a user types into would be able to accept on the engine's
    // port. It is closed again here now that the handover is over.
    unsafe {
        let flags = libc::fcntl(raw, libc::F_GETFD);
        if flags >= 0 {
            libc::fcntl(raw, libc::F_SETFD, flags | libc::FD_CLOEXEC);
        }
    }
    // SAFETY: the descriptor was just verified to be a listening socket of this
    // process, and nothing else owns it here. `from_raw_fd` takes ownership, so
    // it is closed when the listener is dropped.
    let std_listener = unsafe { std::net::TcpListener::from_raw_fd(raw) };
    if std_listener.set_nonblocking(true).is_err() {
        return None;
    }
    tokio::net::TcpListener::from_std(std_listener).ok()
}

/// Whether `fd` is this process's listening TCP socket.
///
/// The kernel answers both questions, so an inherited number is never trusted on
/// its own: `SO_ACCEPTCONN` says it is listening, and a successful
/// `getsockname` on a TCP socket says it is the right kind of descriptor.
fn is_listening_socket(fd: std::os::unix::io::RawFd) -> bool {
    let mut accepting: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_ACCEPTCONN,
            &mut accepting as *mut libc::c_int as *mut libc::c_void,
            &mut len,
        )
    };
    if rc < 0 || accepting == 0 {
        return false;
    }
    let mut addr: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    let mut addr_len = std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockname(
            fd,
            &mut addr as *mut libc::sockaddr_in as *mut libc::sockaddr,
            &mut addr_len,
        )
    };
    rc == 0 && addr.sin_family as libc::c_int == libc::AF_INET
}

fn chat_func(gateway: Arc<Gateway>) -> aixodia_mobile::ChatFunc {
    Arc::new(move |call: ChatCall| {
        let gateway = gateway.clone();
        Box::pin(async move {
            ACTIVE_CHATS.fetch_add(1, Ordering::SeqCst);
            let result = chat_once(gateway, call).await;
            ACTIVE_CHATS.fetch_sub(1, Ordering::SeqCst);
            // A turn that ends is the only moment a queued update can run, now
            // that nothing polls the release page. It costs one HTTP request
            // when the queue is empty, and is skipped entirely when it is not.
            let _ = selfupdate::apply_pending().await;
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
    match std::env::var_os("HOME").filter(|home| !home.is_empty()) {
        Some(home) => std::path::PathBuf::from(home).join(".aixodia"),
        None => std::path::PathBuf::from("."),
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
