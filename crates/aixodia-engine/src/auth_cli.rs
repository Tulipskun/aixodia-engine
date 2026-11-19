//! `aixodia auth` — the CLI's sign-in, shaped like `gh auth login`.
//!
//! The engine binary is also the operator's CLI (`aixodia update`). A CLI needs
//! a credential but has no browser cookie, so it uses the same device flow as
//! the phone: a short user code approved in a browser, then one opaque bearer
//! token from the auth Worker. Nothing here reads `CF_TOKEN`: that is the
//! engine's own credential for the Cloudflare API and the internal socket hubs,
//! and it never leaves this machine.
//!
//! The token is kept in the engine's state directory with mode 0600, the same
//! fallback `gh` uses when no system credential store is available. Headless
//! hosts rarely run a D-Bus secret service, and a private file is still a far
//! better home than an environment variable shared with every child process.

use std::io::Write as _;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::Value;

const DEFAULT_AUTH_URL: &str = "https://aixodia-auth.aixodia.workers.dev";
const DEVICE_CODE_PATH: &str = "/auth/device/code";
const DEVICE_TOKEN_PATH: &str = "/auth/device/token";
const VERIFY_PATH: &str = "/auth/verify";
const REVOKE_PATH: &str = "/auth/revoke";

const POLL_TIMEOUT: Duration = Duration::from_secs(180);

/// Where the CLI's bearer token lives.
pub fn token_path() -> PathBuf {
    aixodia_state::path("cli.token")
}

fn auth_url() -> String {
    std::env::var("AIXODIA_AUTH_URL")
        .unwrap_or_default()
        .trim()
        .trim_end_matches('/')
        .to_string()
        .pipe(|url| {
            if url.is_empty() {
                DEFAULT_AUTH_URL.to_string()
            } else {
                url
            }
        })
}

/// Tiny helper so the fallback above stays readable in one expression.
trait Pipe: Sized {
    fn pipe<T>(self, f: impl FnOnce(Self) -> T) -> T {
        f(self)
    }
}
impl<T> Pipe for T {}

fn read_token() -> Option<String> {
    std::fs::read_to_string(token_path())
        .ok()
        .map(|token| token.trim().to_string())
        .filter(|token| !token.is_empty())
}

fn save_token(token: &str) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt as _;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(token_path())?;
    file.write_all(token.trim().as_bytes())?;
    file.write_all(b"\n")?;
    Ok(())
}

fn forget_token() -> std::io::Result<()> {
    match std::fs::remove_file(token_path()) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

fn client() -> anyhow::Result<reqwest::Client> {
    Ok(aixodia_net::client_builder().build()?)
}

/// The `aixodia auth` entry point. Returns a process exit code.
pub async fn run(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some("login") => login().await,
        Some("status") => status().await,
        Some("token") => show_token(),
        Some("logout") => logout().await,
        Some(other) => {
            eprintln!("aixodia auth: unknown command {other:?} (login, status, token, logout)");
            2
        }
        None => {
            eprintln!("usage: aixodia auth <login|status|token|logout>");
            2
        }
    }
}

async fn login() -> i32 {
    let http = match client() {
        Ok(client) => client,
        Err(err) => return fail(err),
    };
    let url = auth_url();

    let created: Value = match http
        .post(format!("{url}{DEVICE_CODE_PATH}"))
        .json(&serde_json::json!({ "device_name": "aixodia-cli" }))
        .send()
        .await
        .and_then(|res| res.error_for_status())
    {
        Ok(res) => match res.json().await {
            Ok(body) => body,
            Err(err) => return fail(err),
        },
        Err(err) => return fail(err),
    };

    let Some(user_code) = created.get("user_code").and_then(Value::as_str) else {
        return fail(anyhow::anyhow!("the sign-in service sent no user code"));
    };
    let Some(verification_url) = created.get("verification_url").and_then(Value::as_str) else {
        return fail(anyhow::anyhow!("the sign-in service sent no approval page"));
    };
    let Some(device_code) = created.get("device_code").and_then(Value::as_str) else {
        return fail(anyhow::anyhow!("the sign-in service sent no device code"));
    };
    let interval = created
        .get("interval")
        .and_then(Value::as_u64)
        .unwrap_or(5)
        .clamp(2, 30);
    let open_url = format!("{verification_url}?code={device_code}");

    println!("Open this address in a browser and approve the sign-in:");
    println!("  {open_url}");
    println!("Enter this code: {user_code}");
    let _ = std::process::Command::new("xdg-open")
        .arg(&open_url)
        .spawn();

    let started = std::time::Instant::now();
    loop {
        if started.elapsed() >= POLL_TIMEOUT {
            eprintln!("aixodia auth: still waiting for approval; run `aixodia auth login` again");
            return 1;
        }
        tokio::time::sleep(Duration::from_secs(interval)).await;
        let response = match http
            .post(format!("{url}{DEVICE_TOKEN_PATH}"))
            .json(&serde_json::json!({ "device_code": device_code }))
            .send()
            .await
        {
            Ok(res) => res,
            Err(err) => {
                // A poll is a question that can be asked again: a DNS dip must
                // not throw away a code the user is approving right now.
                tracing::warn!("auth poll: {err}");
                continue;
            }
        };
        let status = response.status();
        let body: Value = response.json().await.unwrap_or_default();
        let token = body.get("access_token").and_then(Value::as_str);
        if status.is_success() && token.is_some() {
            if let Err(err) = save_token(token.unwrap_or_default()) {
                return fail(err);
            }
            println!("Signed in. Token saved to {}", token_path().display());
            return 0;
        }
        match body
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "pending" | "authorization_pending" => continue,
            "slow_down" => {
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
            "delivered" => {
                eprintln!("aixodia auth: this code was already used; start a new sign-in");
                return 1;
            }
            "expired" | "denied" | "revoked" => {
                eprintln!(
                    "aixodia auth: the sign-in was {}. Run `aixodia auth login` again.",
                    body.get("status")
                        .and_then(Value::as_str)
                        .unwrap_or("rejected")
                );
                return 1;
            }
            other => {
                eprintln!(
                    "aixodia auth: sign-in failed ({})",
                    if other.is_empty() { "no answer" } else { other }
                );
                let _ = status;
                return 1;
            }
        }
    }
}

async fn status() -> i32 {
    let Some(token) = read_token() else {
        println!("Not signed in. Run `aixodia auth login`.");
        return 1;
    };
    let http = match client() {
        Ok(client) => client,
        Err(err) => return fail(err),
    };
    let response = http
        .post(format!("{}{VERIFY_PATH}", auth_url()))
        .bearer_auth(&token)
        .send()
        .await;
    match response {
        Ok(res) if res.status().is_success() => {
            let body: Value = res.json().await.unwrap_or_default();
            println!(
                "Signed in{}",
                body.get("email")
                    .and_then(Value::as_str)
                    .map(|email| format!(" as {email}"))
                    .unwrap_or_default()
            );
            0
        }
        Ok(res) => {
            println!(
                "Not signed in (auth service answered {}). Run `aixodia auth login`.",
                res.status()
            );
            1
        }
        Err(err) => fail(err),
    }
}

fn show_token() -> i32 {
    match read_token() {
        Some(token) => {
            println!("{token}");
            0
        }
        None => {
            eprintln!("aixodia auth: not signed in");
            1
        }
    }
}

async fn logout() -> i32 {
    let Some(token) = read_token() else {
        println!("Not signed in.");
        return 0;
    };
    let http = match client() {
        Ok(client) => client,
        Err(err) => return fail(err),
    };
    // A failed revoke must not strand a token the user asked to forget: the
    // local copy goes either way, and the Worker drops it if the call lands.
    let _ = http
        .post(format!("{}{REVOKE_PATH}", auth_url()))
        .bearer_auth(&token)
        .send()
        .await;
    match forget_token() {
        Ok(()) => {
            println!("Signed out. The token was revoked where the service could be reached.");
            0
        }
        Err(err) => fail(err),
    }
}

fn fail(err: impl std::fmt::Display) -> i32 {
    eprintln!("aixodia auth: {err}");
    1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_home_is_the_documented_worker() {
        // No override in the test process: the app and the CLI must agree on
        // where a user signs in.
        if std::env::var_os("AIXODIA_AUTH_URL").is_none() {
            assert_eq!(auth_url(), DEFAULT_AUTH_URL);
        }
    }
}
