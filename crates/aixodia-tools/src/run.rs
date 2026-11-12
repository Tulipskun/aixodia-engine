//! Tool execution: `shell`, `set_directory`, `read`, `edit`, `write`,
//! `current_time`, `fetch_url`, and the mirrored goal/plan tools.
//!
//! Port of the execution half of the Go `tools` package.

use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use aixodia_goals::{Kind, Store};
use aixodia_provider::ToolDef;
use anyhow::{anyhow, bail, Context, Result};
use base64::Engine as _;
use chrono::{SecondsFormat, Utc};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use tokio::io::AsyncReadExt;

/// What a tool run needs to know about the chat it belongs to.
#[derive(Clone, Default)]
pub struct ToolContext {
    /// Goals and plans are scoped to the conversation that created them.
    pub session_id: String,
    pub store: Option<Arc<dyn Store>>,
}

type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;
type Runner = Arc<dyn Fn(ToolContext, String) -> BoxFuture<Result<String>> + Send + Sync>;

/// One tool: its declaration and how it runs.
#[derive(Clone)]
pub struct Tool {
    pub def: ToolDef,
    runner: Runner,
}

impl Tool {
    pub async fn run(&self, ctx: ToolContext, args: &str) -> Result<String> {
        (self.runner)(ctx, args.to_string()).await
    }
}

impl std::fmt::Debug for Tool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tool").field("def", &self.def).finish()
    }
}

fn tool(
    def: ToolDef,
    run: impl Fn(ToolContext, String) -> BoxFuture<Result<String>> + Send + Sync + 'static,
) -> Tool {
    Tool {
        def,
        runner: Arc::new(run),
    }
}

fn ready(result: Result<String>) -> BoxFuture<Result<String>> {
    Box::pin(async move { result })
}

/// Adds the one parameter every ordinary tool shares: the model has to say
/// briefly why it is calling the tool. Goal and plan tools are exempt.
pub fn with_reason(mut def: ToolDef) -> ToolDef {
    if def.name.starts_with("goal_") || def.name.starts_with("plan_") {
        return def;
    }
    let Some(properties) = def
        .parameters
        .get_mut("properties")
        .and_then(Value::as_object_mut)
    else {
        return def;
    };
    if properties.contains_key("reason") {
        return def;
    }
    properties.insert(
        "reason".to_string(),
        json!({
            "type": "string",
            "description": "A short reason for using this tool: why it is needed and what it is meant to accomplish."
        }),
    );
    let required = def
        .parameters
        .entry("required")
        .or_insert_with(|| Value::Array(Vec::new()));
    match required {
        Value::Array(items) => {
            if !items.iter().any(|item| item == "reason") {
                items.push(Value::from("reason"));
            }
        }
        _ => *required = Value::Array(vec![Value::from("reason")]),
    }
    def
}

/// The full registry the running engine offers, with the objective tools when a
/// store is configured.
pub fn tools_with(store: Option<Arc<dyn Store>>) -> std::collections::HashMap<String, Tool> {
    let mut out = std::collections::HashMap::new();
    for (name, def) in crate::builtin() {
        let def = with_reason(def);
        let running = match name.as_str() {
            "shell" => tool(def, |ctx, args| {
                Box::pin(async move { run_shell(&ctx, &args).await })
            }),
            "set_directory" => tool(def, |ctx, args| {
                Box::pin(async move { set_directory(&ctx, &args).await })
            }),
            "read" => tool(def, |_, args| {
                Box::pin(async move { read_file(&args).await })
            }),
            "edit" => tool(def, |_, args| {
                Box::pin(async move { run_edit(&args).await })
            }),
            "write" => tool(def, |_, args| {
                Box::pin(async move { run_write(&args).await })
            }),
            "current_time" => tool(def, |_, args| {
                Box::pin(async move { current_time(&args).await })
            }),
            "fetch_url" => tool(def, |_, args| {
                Box::pin(async move { fetch_url(&args).await })
            }),
            other => {
                let name = other.to_string();
                tool(def, move |_, _| {
                    let name = name.clone();
                    ready(Err(anyhow!("unknown tool {name}")))
                })
            }
        };
        out.insert(name, running);
    }

    for (name, def) in [
        ("goal_add", objective_add_def(Kind::Goal)),
        ("goal_edit", objective_edit_def(Kind::Goal)),
        ("goal_complete", objective_complete_def(Kind::Goal)),
        ("goal_drop", objective_drop_def(Kind::Goal)),
        ("plan_add", objective_add_def(Kind::Plan)),
        ("plan_edit", objective_edit_def(Kind::Plan)),
        ("plan_complete", objective_complete_def(Kind::Plan)),
        ("plan_drop", objective_drop_def(Kind::Plan)),
    ] {
        let kind = if name.starts_with("goal_") {
            Kind::Goal
        } else {
            Kind::Plan
        };
        let noun = kind.as_str().to_string();
        let def = with_reason(def);
        let entry = if name.ends_with("_add") {
            let store = store.clone();
            let noun = noun.clone();
            tool(def, move |ctx, args| {
                let store = store.clone();
                let noun = noun.clone();
                Box::pin(async move { objective_add(store, ctx, kind, &noun, &args).await })
            })
        } else if name.ends_with("_edit") {
            let store = store.clone();
            let noun = noun.clone();
            tool(def, move |ctx, args| {
                let store = store.clone();
                let noun = noun.clone();
                Box::pin(async move { objective_edit(store, ctx, kind, &noun, &args).await })
            })
        } else if name.ends_with("_complete") {
            let store = store.clone();
            let noun = noun.clone();
            tool(def, move |ctx, args| {
                let store = store.clone();
                let noun = noun.clone();
                Box::pin(async move {
                    objective_modify(store, ctx, kind, &noun, &args, "complete").await
                })
            })
        } else {
            let store = store.clone();
            let noun = noun.clone();
            tool(def, move |ctx, args| {
                let store = store.clone();
                let noun = noun.clone();
                Box::pin(
                    async move { objective_modify(store, ctx, kind, &noun, &args, "drop").await },
                )
            })
        };
        out.insert(name.to_string(), entry);
    }
    out
}

/// Runs one builtin by name with no chat context. The six context-free tools
/// work; the objective tools report that they need a session.
pub async fn run_tool(name: &str, args: &str) -> Result<String> {
    let tools = tools_with(None);
    let Some(tool) = tools.get(name) else {
        bail!("unknown tool {name}");
    };
    tool.run(ToolContext::default(), args).await
}

// ---------------------------------------------------------------- objectives

fn objective_add_def(kind: Kind) -> ToolDef {
    let noun = kind.as_str();
    let id_field = format!("{noun}_id");
    let mut description = "Record the user's overall goal before working on it. Use this when the user asks \
        for something that will take several steps, so the objective survives across tool calls and turns. \
        Use plan_add for the separate steps. Returns the new "
        .to_string();
    if kind == Kind::Plan {
        description =
            "Record one step toward the current goal. Plans are a separate list from goals. \
            Returns the new "
                .to_string();
    }
    description.push_str(&id_field);
    description.push('.');
    ToolDef {
        name: format!("{noun}_add"),
        description,
        parameters: object(json!({
            "properties": {
                "content": {
                    "type": "string",
                    "description": "What has to be achieved or done, in a short sentence."
                }
            },
            "required": ["content"]
        })),
    }
}

fn objective_edit_def(kind: Kind) -> ToolDef {
    let noun = kind.as_str();
    let id_field = format!("{noun}_id");
    ToolDef {
        name: format!("{noun}_edit"),
        description: format!(
            "Replace the content of an open {noun}. Use this when the user changes what it should be."
        ),
        parameters: object(json!({
            "properties": {
                id_field.clone(): {
                    "type": "string",
                    "description": format!("The {id_field} returned by {noun}_add.")
                },
                "content": {
                    "type": "string",
                    "description": format!("The complete new content of the {noun}.")
                }
            },
            "required": [id_field, "content"]
        })),
    }
}

fn objective_complete_def(kind: Kind) -> ToolDef {
    let noun = kind.as_str();
    let id_field = format!("{noun}_id");
    ToolDef {
        name: format!("{noun}_complete"),
        description: format!(
            "Mark a {noun} complete once its work is actually finished. It is removed from the active list. Returns a confirmation."
        ),
        parameters: object(json!({
            "properties": {
                id_field.clone(): {
                    "type": "string",
                    "description": format!("The {id_field} returned by {noun}_add.")
                }
            },
            "required": [id_field]
        })),
    }
}

fn objective_drop_def(kind: Kind) -> ToolDef {
    let noun = kind.as_str();
    let id_field = format!("{noun}_id");
    ToolDef {
        name: format!("{noun}_drop"),
        description: format!(
            "Mark a {noun} dropped when the user cancels it. It is removed from the active list. Returns a confirmation."
        ),
        parameters: object(json!({
            "properties": {
                id_field.clone(): {
                    "type": "string",
                    "description": format!("The {id_field} returned by {noun}_add.")
                }
            },
            "required": [id_field]
        })),
    }
}

fn object(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(mut map) => {
            map.insert("type".to_string(), Value::from("object"));
            map
        }
        _ => Map::new(),
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ObjectiveInput {
    goal_id: String,
    plan_id: String,
    content: String,
}

fn objective_id(kind: Kind, input: &ObjectiveInput) -> String {
    match kind {
        Kind::Goal => input.goal_id.clone(),
        Kind::Plan => input.plan_id.clone(),
    }
}

fn objective_store(
    store: Option<Arc<dyn Store>>,
    ctx: &ToolContext,
) -> Result<(Arc<dyn Store>, String)> {
    let store = store.ok_or_else(|| anyhow!("objective storage is not configured"))?;
    let session = ctx.session_id.trim();
    if session.is_empty() {
        bail!("goal and plan tools need an active chat session");
    }
    Ok((store, session.to_string()))
}

async fn objective_add(
    store: Option<Arc<dyn Store>>,
    ctx: ToolContext,
    kind: Kind,
    noun: &str,
    args: &str,
) -> Result<String> {
    let input: ObjectiveInput = parse_args(args)?;
    let (store, session) = objective_store(store, &ctx)?;
    let id = store.add(&session, kind, &input.content).await?;
    sync_objectives_file(&store, &session).await;
    Ok(format!("{noun}_id: {id}"))
}

async fn objective_edit(
    store: Option<Arc<dyn Store>>,
    ctx: ToolContext,
    kind: Kind,
    noun: &str,
    args: &str,
) -> Result<String> {
    let input: ObjectiveInput = parse_args(args)?;
    let id = objective_id(kind, &input);
    let (store, session) = objective_store(store, &ctx)?;
    store.edit(&session, kind, &id, &input.content).await?;
    sync_objectives_file(&store, &session).await;
    Ok(format!("{noun} {id} updated"))
}

async fn objective_modify(
    store: Option<Arc<dyn Store>>,
    ctx: ToolContext,
    kind: Kind,
    noun: &str,
    args: &str,
    action: &str,
) -> Result<String> {
    let input: ObjectiveInput = parse_args(args)?;
    let id = objective_id(kind, &input);
    let (store, session) = objective_store(store, &ctx)?;
    let word = if action == "complete" {
        store.complete(&session, kind, &id).await?;
        "completed"
    } else {
        store.drop_item(&session, kind, &id).await?;
        "dropped"
    };
    sync_objectives_file(&store, &session).await;
    Ok(format!("{noun} {id} {word}"))
}

/// Rebuilds the Obsidian mirror in the chat's set directory after a successful
/// mutation. A chat with no directory writes nothing; a failed write does not
/// fail the tool call, because the store is the source of truth.
async fn sync_objectives_file(store: &Arc<dyn Store>, session: &str) {
    let Some(dir) = crate::dir::get(session) else {
        return;
    };
    match store.all(session).await {
        Ok(items) => {
            if let Err(err) = crate::obsidian::write_goals_md(&dir, session, &items).await {
                tracing::warn!("tools: goals file: {err}");
            }
        }
        Err(err) => tracing::warn!("tools: goals list for file: {err}"),
    }
}

fn parse_args<T: for<'de> Deserialize<'de>>(args: &str) -> Result<T> {
    serde_json::from_str(args).with_context(|| "invalid arguments")
}

// --------------------------------------------------------------------- shell

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ShellInput {
    command: String,
    timeout: f64,
    workdir: String,
}

async fn run_shell(ctx: &ToolContext, args: &str) -> Result<String> {
    let input: ShellInput = parse_args(args)?;
    if input.command.trim().is_empty() {
        bail!("command must not be empty");
    }
    let mut timeout = Duration::from_secs(120);
    if input.timeout > 0.0 {
        timeout = Duration::from_secs_f64(input.timeout / 1000.0);
        if timeout > Duration::from_secs(300) {
            timeout = Duration::from_secs(300);
        }
        if timeout < Duration::from_secs(1) {
            timeout = Duration::from_secs(1);
        }
    }

    // An explicit `workdir` is a one-off override; otherwise the directory the
    // chat set with `set_directory` wins over the home directory.
    let workdir = if input.workdir.trim().is_empty() {
        crate::dir::get(&ctx.session_id)
    } else {
        Some(PathBuf::from(input.workdir.trim()))
    };

    let mut command = tokio::process::Command::new("sh");
    command.arg("-c").arg(&input.command);
    if let Some(workdir) = workdir {
        command.current_dir(workdir);
    } else if let Some(home) = home_dir() {
        if home.is_dir() {
            command.current_dir(home);
        }
    }
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    // Put the shell in its own process group so a timeout kills the whole tree,
    // and point stderr at the same pipe as stdout so output keeps its order.
    #[cfg(unix)]
    unsafe {
        command.pre_exec(|| {
            libc::setpgid(0, 0);
            libc::dup2(1, 2);
            Ok(())
        });
    }

    let mut child = command.spawn().context("cannot start shell")?;
    let pid = child.id();
    let stdout = child.stdout.take().expect("piped stdout");
    let buffer = Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
    let sink = buffer.clone();
    let reader = tokio::spawn(async move {
        let mut stdout = stdout;
        let mut chunk = [0u8; 8192];
        loop {
            match stdout.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => sink.lock().unwrap().extend_from_slice(&chunk[..n]),
            }
        }
    });

    let waited = tokio::time::timeout(timeout, child.wait()).await;
    let timed_out = waited.is_err();
    if timed_out {
        if let Some(pid) = pid {
            #[cfg(unix)]
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
        }
        let _ = child.start_kill();
        let _ = child.wait().await;
    }
    let _ = reader.await;
    let bytes = buffer.lock().unwrap().clone();
    let mut text = String::from_utf8_lossy(&bytes).to_string();
    if text.len() > 32 * 1024 {
        text.truncate(floor_char_boundary(&text, 32 * 1024));
        text.push_str("\n[truncated]");
    }

    if timed_out {
        return Err(anyhow!("command timed out after {}s", timeout.as_secs()));
    }
    match waited {
        Ok(Ok(status)) if status.success() => Ok(text),
        Ok(Ok(status)) => Ok(format!("{text}exit code {}", status.code().unwrap_or(-1))),
        Ok(Err(err)) => Err(err.into()),
        Err(_) => unreachable!("handled above"),
    }
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

fn floor_char_boundary(text: &str, mut index: usize) -> usize {
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

// ------------------------------------------------------------- set_directory

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct SetDirectoryInput {
    path: String,
}

async fn set_directory(ctx: &ToolContext, args: &str) -> Result<String> {
    let input: SetDirectoryInput = parse_args(args)?;
    let raw = input.path.trim();
    if raw.is_empty() {
        crate::dir::clear(&ctx.session_id);
        return Ok(
            "shell working directory cleared; commands run in the home directory again".to_string(),
        );
    }

    // A relative path is read as relative to where the shell already is, which
    // is what the model means when it says "go one level up".
    let wanted = Path::new(raw);
    let candidate = if wanted.is_absolute() {
        wanted.to_path_buf()
    } else {
        let base = crate::dir::get(&ctx.session_id)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("/"));
        base.join(wanted)
    };

    // Canonicalising resolves `..` and symlinks and fails when nothing is
    // there, so the shell never starts in a directory that cannot exist.
    let resolved = tokio::fs::canonicalize(&candidate)
        .await
        .with_context(|| format!("cannot use {raw} as a working directory"))?;
    let meta = tokio::fs::metadata(&resolved)
        .await
        .with_context(|| format!("cannot stat {}", resolved.display()))?;
    if !meta.is_dir() {
        bail!("{raw} is not a directory");
    }
    crate::dir::set(&ctx.session_id, &resolved);
    // A directory set late in a chat still gets the mirror of what exists.
    if let Some(store) = &ctx.store {
        sync_objectives_file(store, ctx.session_id.trim()).await;
    }
    Ok(format!(
        "shell working directory set to {}",
        resolved.display()
    ))
}

// ---------------------------------------------------------------------- read

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ReadInput {
    path: String,
    #[serde(rename = "filePath")]
    file_path: String,
    offset: f64,
    limit: f64,
    base64: bool,
}

const BLOCKED_DIRS: [&str; 3] = ["/proc", "/sys", "/dev"];
const MAX_READ_BYTES: usize = 256 * 1024;
const MAX_READ_FILE: u64 = 4 * 1024 * 1024;
const MAX_LINE_CHARS: usize = 2000;
const BASE64_CEILING: usize = 192 * 1024;

async fn read_file(args: &str) -> Result<String> {
    let input: ReadInput = parse_args(args)?;
    let raw_path = if input.file_path.trim().is_empty() {
        input.path.trim()
    } else {
        input.file_path.trim()
    };
    if raw_path.is_empty() {
        bail!("filePath must not be empty");
    }
    let path = checked_absolute(raw_path, "reading")?;
    let meta = tokio::fs::metadata(&path)
        .await
        .with_context(|| format!("cannot stat {raw_path}"))?;
    if meta.is_dir() {
        let mut entries = tokio::fs::read_dir(&path)
            .await
            .with_context(|| format!("cannot list {raw_path}"))?;
        let mut names: Vec<String> = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            let file_type = entry.file_type().await?;
            let mut name = entry.file_name().to_string_lossy().to_string();
            if file_type.is_dir() {
                name.push('/');
            }
            names.push(name);
        }
        names.sort();
        return Ok(names.into_iter().map(|name| format!("{name}\n")).collect());
    }
    if !meta.is_file() {
        bail!("{raw_path} is not a regular file");
    }

    if input.base64 {
        return encode_base64_file(&path, meta.len()).await;
    }

    if meta.len() > MAX_READ_FILE {
        bail!(
            "{raw_path} is too large ({:.1} MB)",
            meta.len() as f64 / 1048576.0
        );
    }
    let file = tokio::fs::File::open(&path)
        .await
        .with_context(|| format!("cannot open {raw_path}"))?;
    let mut raw = Vec::new();
    file.take(MAX_READ_BYTES as u64 + 1)
        .read_to_end(&mut raw)
        .await
        .with_context(|| format!("cannot read {raw_path}"))?;

    if let Some(kind) = binary_kind(&path, &raw) {
        bail!(
            "{raw_path} is a {kind} file, not text. The read tool only returns text, so the contents cannot be shown here."
        );
    }

    let truncated = raw.len() > MAX_READ_BYTES;
    if truncated {
        raw.truncate(MAX_READ_BYTES);
    }
    let text = String::from_utf8_lossy(&raw).to_string();
    let mut offset = input.offset as i64;
    if offset < 1 {
        offset = 1;
    }
    let mut limit = input.limit as i64;
    if limit < 1 {
        limit = 2000;
    }
    let mut lines: Vec<&str> = text.split('\n').collect();
    if lines.last() == Some(&"") {
        lines.pop();
    }
    if offset as usize > lines.len() {
        bail!("offset {offset} beyond end of file ({} lines)", lines.len());
    }
    let start = offset as usize - 1;
    let end = (start + limit as usize).min(lines.len());
    let mut out = String::new();
    for (index, line) in lines[start..end].iter().enumerate() {
        let line: String = line.chars().take(MAX_LINE_CHARS).collect();
        out.push_str(&format!("{}: {}\n", offset as usize + index, line));
    }
    if truncated || end < lines.len() {
        out.push_str("[truncated]");
    }
    Ok(out)
}

fn checked_absolute(raw_path: &str, verb: &str) -> Result<PathBuf> {
    let path = Path::new(raw_path);
    if !path.is_absolute() {
        bail!("filePath must be absolute");
    }
    let clean = path.to_path_buf();
    for blocked in BLOCKED_DIRS {
        if clean == Path::new(blocked) || clean.starts_with(blocked) {
            bail!("{verb} {blocked} is not allowed");
        }
    }
    Ok(clean)
}

fn binary_names(extension: &str) -> Option<&'static str> {
    Some(match extension {
        ".jpg" | ".jpeg" => "JPEG image",
        ".png" => "PNG image",
        ".gif" => "GIF image",
        ".webp" => "WebP image",
        ".bmp" => "BMP image",
        ".heic" => "HEIC image",
        ".tiff" => "TIFF image",
        ".mp4" => "MP4 video",
        ".mov" => "QuickTime video",
        ".mkv" => "Matroska video",
        ".mp3" => "MP3 audio",
        ".wav" => "WAV audio",
        ".m4a" => "M4A audio",
        ".pdf" => "PDF document",
        ".zip" => "ZIP archive",
        ".gz" => "gzip archive",
        ".apk" => "Android package",
        ".so" => "shared library",
        _ => return None,
    })
}

/// Reports whether a file cannot be read as text, and what it is.
fn binary_kind(path: &Path, raw: &[u8]) -> Option<&'static str> {
    let extension = path
        .extension()
        .map(|ext| format!(".{}", ext.to_string_lossy().to_lowercase()))
        .unwrap_or_default();
    if let Some(kind) = binary_names(&extension) {
        return Some(kind);
    }
    let head = &raw[..raw.len().min(4096)];
    if head.is_empty() {
        return None;
    }
    if head.contains(&0) || std::str::from_utf8(head).is_err() {
        return Some("binary");
    }
    None
}

async fn encode_base64_file(path: &Path, size: u64) -> Result<String> {
    const MAX_ENCODE_BYTES: u64 = 32 * 1024 * 1024;
    if size > MAX_ENCODE_BYTES {
        bail!(
            "{} is too large to encode ({:.1} MB, ceiling {:.0} MB)",
            path.display(),
            size as f64 / 1048576.0,
            MAX_ENCODE_BYTES as f64 / 1048576.0
        );
    }
    let raw = tokio::fs::read(path)
        .await
        .with_context(|| format!("cannot read {}", path.display()))?;
    Ok(encode_base64(path, &raw))
}

fn encode_base64(path: &Path, raw: &[u8]) -> String {
    let shown = path.display().to_string();
    // The ceiling counts characters and base64 spends four for every three
    // bytes, so the budget handed to the shrinker is scaled back.
    if let Ok(pic) = aixodia_imageprep::prepare_within(raw, BASE64_CEILING * 3 / 4) {
        let note = if pic.resized {
            format!(
                "{}x{}, reduced from {}x{}",
                pic.width, pic.height, pic.original_width, pic.original_height
            )
        } else {
            format!("{}x{}", pic.width, pic.height)
        };
        return data_uri_result(&shown, &note, &pic.media_type, &pic.data);
    }
    data_uri_result(
        &shown,
        &format!("{} bytes", raw.len()),
        guess_media_type(path),
        raw,
    )
}

fn data_uri_result(path: &str, note: &str, media_type: &str, raw: &[u8]) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(raw);
    let header = format!("{path} — {media_type} ({note})");
    if encoded.len() > BASE64_CEILING {
        return format!(
            "{header}\ntoo large to return: {} characters of base64, ceiling is {BASE64_CEILING}. Use the bash tool to process it on the host instead.",
            encoded.len()
        );
    }
    format!("{header}\ndata:{media_type};base64,{encoded}")
}

fn guess_media_type(path: &Path) -> &'static str {
    let extension = path
        .extension()
        .map(|ext| format!(".{}", ext.to_string_lossy().to_lowercase()))
        .unwrap_or_default();
    match extension.as_str() {
        ".pdf" => "application/pdf",
        ".png" => "image/png",
        ".gif" => "image/gif",
        ".webp" => "image/webp",
        ".mp4" => "video/mp4",
        ".txt" | ".md" | ".json" | ".csv" | ".log" => "text/plain",
        _ => "application/octet-stream",
    }
}

// ---------------------------------------------------------------------- edit

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct EditInput {
    path: String,
    #[serde(rename = "filePath")]
    file_path: String,
    #[serde(rename = "oldString")]
    old_string: String,
    #[serde(rename = "newString")]
    new_string: String,
}

async fn run_edit(args: &str) -> Result<String> {
    let input: EditInput = parse_args(args)?;
    let raw_path = if input.file_path.trim().is_empty() {
        input.path.trim()
    } else {
        input.file_path.trim()
    };
    if raw_path.is_empty() {
        bail!("filePath must not be empty");
    }
    let path = checked_absolute(raw_path, "editing")?;
    let meta = tokio::fs::metadata(&path)
        .await
        .with_context(|| format!("cannot stat {raw_path}"))?;
    if meta.is_dir() {
        bail!("{raw_path} is a directory");
    }
    let raw = tokio::fs::read(&path)
        .await
        .with_context(|| format!("cannot read {raw_path}"))?;
    let text = String::from_utf8_lossy(&raw).to_string();
    if text == input.old_string && input.old_string == input.new_string {
        bail!("oldString and newString must differ");
    }
    let count = text.matches(&input.old_string).count();
    if count == 0 {
        bail!("oldString not found in {raw_path}");
    }
    if count > 1 {
        bail!("oldString occurs {count} times in {raw_path}; add more context to make it unique");
    }
    let new_text = text.replacen(&input.old_string, &input.new_string, 1);
    let permissions = meta.permissions();
    tokio::fs::write(&path, new_text.as_bytes())
        .await
        .with_context(|| format!("cannot write {raw_path}"))?;
    // write() keeps an existing file's mode; restore it in case the platform
    // changed it.
    let _ = tokio::fs::set_permissions(&path, permissions).await;
    Ok(format!("edited {raw_path}: {count} replacement(s)"))
}

// --------------------------------------------------------------------- write

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct WriteInput {
    path: String,
    #[serde(rename = "filePath")]
    file_path: String,
    content: String,
}

async fn run_write(args: &str) -> Result<String> {
    let input: WriteInput = parse_args(args)?;
    let raw_path = if input.file_path.trim().is_empty() {
        input.path.trim()
    } else {
        input.file_path.trim()
    };
    if raw_path.is_empty() {
        bail!("filePath must not be empty");
    }
    let path = checked_absolute(raw_path, "writing")?;
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("cannot create parent directory for {raw_path}"))?;
    }
    tokio::fs::write(&path, input.content.as_bytes())
        .await
        .with_context(|| format!("cannot write {raw_path}"))?;
    Ok(format!("wrote {} bytes to {raw_path}", input.content.len()))
}

// -------------------------------------------------------------- current_time

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct TimeInput {
    timezone: String,
}

async fn current_time(args: &str) -> Result<String> {
    let input: TimeInput = if args.trim().is_empty() {
        TimeInput::default()
    } else {
        parse_args(args)?
    };
    if input.timezone.is_empty() {
        return Ok(Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true));
    }
    let zone: chrono_tz::Tz = input
        .timezone
        .parse()
        .map_err(|_| anyhow!("unknown time zone {:?}", input.timezone))?;
    Ok(Utc::now()
        .with_timezone(&zone)
        .to_rfc3339_opts(SecondsFormat::Secs, false))
}

// ------------------------------------------------------------------ fetch_url

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct FetchInput {
    url: String,
}

async fn fetch_url(args: &str) -> Result<String> {
    let input: FetchInput = parse_args(args)?;
    let mut current = validate_url(&input.url)?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);

    // Redirects are followed by hand so every hop is checked after DNS
    // resolution, which is what stops a rebinding answer from reaching a local
    // service or a cloud metadata endpoint.
    for _hop in 0..5 {
        let client = public_client(&current).await?;
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            bail!("fetch timed out after 15s");
        }
        let response = client
            .get(current.clone())
            .timeout(remaining)
            .send()
            .await?;
        let status = response.status();
        if status.is_redirection() {
            if let Some(location) = response.headers().get(reqwest::header::LOCATION) {
                let location = location.to_str().unwrap_or_default();
                let next = current.join(location)?;
                current = validate_url(next.as_str())?;
                continue;
            }
        }
        let mut body = Vec::new();
        let mut response = response;
        while let Some(chunk) = response.chunk().await? {
            let room = 128 * 1024 - body.len();
            if room == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..chunk.len().min(room)]);
        }
        let mut text = String::from_utf8_lossy(&body).to_string();
        if text.len() > 32 * 1024 {
            text.truncate(floor_char_boundary(&text, 32 * 1024));
            text.push_str("\n[truncated]");
        }
        return Ok(format!("HTTP {}\n{text}", status.as_u16()));
    }
    bail!("too many redirects")
}

fn validate_url(raw: &str) -> Result<url::Url> {
    let parsed =
        url::Url::parse(raw.trim()).map_err(|_| anyhow!("url must be absolute http or https"))?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        bail!("url must be absolute http or https");
    }
    if let Some(host) = parsed.host_str() {
        let host = host.trim_end_matches('.').to_lowercase();
        if host == "localhost" || host.ends_with(".localhost") || host.ends_with(".local") {
            bail!("local and private network URLs are not allowed");
        }
        if let Ok(ip) = host.parse::<IpAddr>() {
            if !is_public_ip(ip) {
                bail!("local and private network URLs are not allowed");
            }
        }
    }
    Ok(parsed)
}

async fn public_client(target: &url::Url) -> Result<reqwest::Client> {
    let host = target
        .host_str()
        .ok_or_else(|| anyhow!("url must be absolute http or https"))?
        .to_string();
    let port = target
        .port_or_known_default()
        .ok_or_else(|| anyhow!("url must be absolute http or https"))?;
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), port))
        .await
        .with_context(|| format!("cannot resolve {host}"))?
        .collect();
    if addrs.is_empty() {
        bail!("hostname resolved to no addresses");
    }
    for address in &addrs {
        if !is_public_ip(address.ip()) {
            bail!("local and private network URLs are not allowed");
        }
    }
    let first = addrs[0];
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(15))
        .resolve(&host, first)
        .build()
        .map_err(Into::into)
}

fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => is_public_v6(v6),
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    if ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_multicast()
        || ip.is_documentation()
    {
        return false;
    }
    let value = u32::from(ip);
    const BLOCKED: &[(u32, u32)] = &[
        (0x6440_0000, 0xFFC0_0000), // 100.64.0.0/10, carrier-grade NAT
        (0xC000_0000, 0xFFFF_FF00), // 192.0.0.0/24
        (0xC612_0000, 0xFFFF_0000), // 198.18.0.0/15, benchmarking
        (0xF000_0000, 0xF000_0000), // 240.0.0.0/4, reserved
    ];
    for (network, mask) in BLOCKED {
        if value & mask == network & mask {
            return false;
        }
    }
    true
}

fn is_public_v6(ip: Ipv6Addr) -> bool {
    if ip.is_unspecified() || ip.is_loopback() || ip.is_multicast() {
        return false;
    }
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_public_v4(v4);
    }
    let segments = ip.segments();
    // Unique local fc00::/7 and link-local fe80::/10.
    if segments[0] & 0xfe00 == 0xfc00 {
        return false;
    }
    if segments[0] & 0xffc0 == 0xfe80 {
        return false;
    }
    // Documentation 2001:db8::/32.
    if segments[0] == 0x2001 && segments[1] == 0x0db8 {
        return false;
    }
    true
}

pub use Tool as ToolHandle;

#[cfg(test)]
mod tests {
    use super::*;
    use aixodia_goals::MemoryStore;
    use serde_json::Value;

    #[test]
    fn reason_is_added_once_and_required() {
        let def = crate::builtin().remove("shell").expect("shell declaration");
        let def = with_reason(def);
        let properties = def.parameters["properties"].as_object().unwrap();
        assert!(properties.contains_key("reason"));
        let required = def.parameters["required"].as_array().unwrap();
        assert!(required.iter().any(|item| item == "reason"));
        // Adding it twice leaves a single entry.
        let again = with_reason(def);
        let required = again.parameters["required"].as_array().unwrap();
        assert_eq!(
            required
                .iter()
                .filter(|item| item.as_str() == Some("reason"))
                .count(),
            1
        );
    }

    #[test]
    fn objective_tools_take_no_reason() {
        let def = objective_add_def(Kind::Goal);
        assert!(!def.parameters["properties"]
            .as_object()
            .unwrap()
            .contains_key("reason"));
    }

    #[tokio::test]
    async fn shell_combines_output_and_reports_the_exit_code() {
        let ctx = ToolContext::default();
        let out = run_shell(&ctx, r#"{"command":"echo hello"}"#)
            .await
            .expect("shell");
        assert_eq!(out, "hello\n");
        let out = run_shell(&ctx, r#"{"command":"echo oops; exit 3"}"#)
            .await
            .expect("shell");
        assert_eq!(out, "oops\nexit code 3");
        assert!(run_shell(&ctx, r#"{"command":""}"#).await.is_err());
    }

    #[tokio::test]
    async fn shell_workdir_and_timeout() {
        let ctx = ToolContext::default();
        let dir = std::env::temp_dir();
        let args = json!({"command": "pwd", "workdir": dir.to_string_lossy()}).to_string();
        let out = run_shell(&ctx, &args).await.expect("shell");
        assert_eq!(out.trim(), dir.to_string_lossy());
        let out = run_shell(&ctx, r#"{"command":"sleep 2","timeout":1}"#).await;
        assert!(out.is_err());
    }

    #[tokio::test]
    async fn set_directory_points_the_shell_at_that_directory() {
        let dir = std::env::temp_dir().join(format!("aixodia-cwd-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let want = std::fs::canonicalize(&dir).expect("canonical");
        let ctx = ToolContext {
            session_id: "cwd-session".to_string(),
            store: None,
        };

        let args = json!({"path": dir.to_string_lossy()}).to_string();
        let out = set_directory(&ctx, &args).await.expect("set");
        assert!(out.contains(&want.display().to_string()), "{out}");

        let out = run_shell(&ctx, r#"{"command":"pwd"}"#)
            .await
            .expect("shell");
        assert_eq!(out.trim(), want.display().to_string());

        // An explicit workdir still wins for that one call.
        let elsewhere = json!({"command": "pwd", "workdir": "/"}).to_string();
        assert_eq!(
            run_shell(&ctx, &elsewhere).await.expect("shell").trim(),
            "/"
        );

        // A relative path is read against the directory already set.
        std::fs::create_dir_all(want.join("nested")).expect("mkdir");
        set_directory(&ctx, r#"{"path":"nested"}"#)
            .await
            .expect("relative");
        let out = run_shell(&ctx, r#"{"command":"pwd"}"#)
            .await
            .expect("shell");
        assert_eq!(out.trim(), want.join("nested").display().to_string());

        // Another session is untouched.
        let other = ToolContext {
            session_id: "cwd-other".to_string(),
            store: None,
        };
        assert!(crate::dir::get(&other.session_id).is_none());

        // An empty path clears the setting.
        set_directory(&ctx, r#"{"path":""}"#).await.expect("clear");
        assert!(crate::dir::get(&ctx.session_id).is_none());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn set_directory_refuses_files_and_missing_paths() {
        let dir = std::env::temp_dir().join(format!("aixodia-cwd-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let file = dir.join("f.txt");
        std::fs::write(&file, "x").expect("write");
        let ctx = ToolContext::default();

        let args = json!({"path": file.to_string_lossy()}).to_string();
        assert!(set_directory(&ctx, &args).await.is_err());
        let args = json!({"path": dir.join("nope").to_string_lossy()}).to_string();
        assert!(set_directory(&ctx, &args).await.is_err());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn read_numbers_lines_and_refuses_binaries() {
        let dir = std::env::temp_dir().join(format!("aixodia-read-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let file = dir.join("a.txt");
        std::fs::write(&file, "one\ntwo\nthree\n").expect("write");
        let out = read_file(&json!({"filePath": file}).to_string())
            .await
            .expect("read");
        assert_eq!(out, "1: one\n2: two\n3: three\n");

        let sliced = read_file(&json!({"filePath": file, "offset": 2, "limit": 1}).to_string())
            .await
            .expect("read");
        assert_eq!(sliced, "2: two\n[truncated]");

        let listing = read_file(&json!({"filePath": dir}).to_string())
            .await
            .expect("list");
        assert!(listing.contains("a.txt"));

        let binary = dir.join("b.bin");
        std::fs::write(&binary, [0u8, 1, 2, 3]).expect("write");
        let err = read_file(&json!({"filePath": binary}).to_string())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("binary"));

        assert!(read_file(r#"{"filePath":"relative.txt"}"#).await.is_err());
        assert!(read_file(r#"{"filePath":"/proc/self/environ"}"#)
            .await
            .is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn edit_requires_a_unique_match() {
        let dir = std::env::temp_dir().join(format!("aixodia-edit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let file = dir.join("e.txt");
        std::fs::write(&file, "a a b").expect("write");
        let args = json!({"filePath": file, "oldString": "a", "newString": "c"}).to_string();
        assert!(run_edit(&args).await.is_err());
        let args = json!({"filePath": file, "oldString": "b", "newString": "c"}).to_string();
        run_edit(&args).await.expect("edit");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "a a c");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn write_creates_parents_and_reports_bytes() {
        let dir = std::env::temp_dir().join(format!("aixodia-write-{}", std::process::id()));
        let file = dir.join("nested/w.txt");
        let args = json!({"filePath": file, "content": "hello"}).to_string();
        let out = run_write(&args).await.expect("write");
        assert!(out.contains("wrote 5 bytes"));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "hello");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn current_time_honours_a_zone() {
        let utc = current_time("{}").await.expect("utc");
        assert!(utc.ends_with('Z'));
        let bangkok = current_time(r#"{"timezone":"Asia/Bangkok"}"#)
            .await
            .expect("bangkok");
        assert!(bangkok.ends_with("+07:00"), "{bangkok}");
        assert!(current_time(r#"{"timezone":"Mars/Olympus"}"#)
            .await
            .is_err());
    }

    #[test]
    fn private_addresses_are_refused() {
        for address in [
            "127.0.0.1",
            "10.1.2.3",
            "192.168.1.1",
            "169.254.1.1",
            "100.64.0.1",
            "0.0.0.0",
            "::1",
            "fe80::1",
            "fc00::1",
        ] {
            assert!(
                !is_public_ip(address.parse().unwrap()),
                "{address} should be private"
            );
        }
        assert!(is_public_ip("1.1.1.1".parse().unwrap()));
        assert!(is_public_ip("2606:4700::1111".parse().unwrap()));
    }

    #[tokio::test]
    async fn fetch_refuses_local_targets() {
        for url in [
            "http://localhost/",
            "http://127.0.0.1/",
            "http://10.0.0.1/",
            "file:///etc/passwd",
        ] {
            assert!(
                fetch_url(&json!({"url": url}).to_string()).await.is_err(),
                "{url} should be refused"
            );
        }
    }

    #[tokio::test]
    async fn objective_tools_use_the_session_store() {
        let store = Arc::new(MemoryStore::default());
        let tools = tools_with(Some(store.clone()));
        let ctx = ToolContext {
            session_id: "chat-1".to_string(),
            store: Some(store.clone()),
        };
        let added = tools["goal_add"]
            .run(ctx.clone(), r#"{"content":"ship it"}"#)
            .await
            .expect("add");
        assert!(added.starts_with("goal_id: "));
        let items = store.open("chat-1").await.expect("open");
        assert_eq!(items.len(), 1);

        let id = items[0].id.clone();
        let args = json!({"goal_id": id, "content": "ship it now"}).to_string();
        tools["goal_edit"]
            .run(ctx.clone(), &args)
            .await
            .expect("edit");
        assert_eq!(
            store.open("chat-1").await.expect("open")[0].content,
            "ship it now"
        );

        // The plan list does not contain the goal.
        assert!(store.open("chat-1").await.expect("open")[0].kind == "goal");
        let missing = tools["plan_add"]
            .run(ToolContext::default(), r#"{"content":"step"}"#)
            .await;
        assert!(missing.is_err());
    }

    #[tokio::test]
    async fn run_tool_dispatches_by_name() {
        let out = run_tool("current_time", "{}").await.expect("time");
        assert!(!out.is_empty());
        assert!(run_tool("nope", "{}").await.is_err());
    }

    #[test]
    fn registry_has_every_declaration() {
        let tools = tools_with(None);
        for name in [
            "shell",
            "set_directory",
            "read",
            "edit",
            "write",
            "current_time",
            "fetch_url",
            "goal_add",
            "plan_drop",
        ] {
            assert!(tools.contains_key(name), "{name} missing");
        }
        let reason = &tools["read"].def.parameters["properties"]["reason"];
        assert_eq!(reason["type"], Value::from("string"));
    }
}
