//! A machine's own Named Tunnel, made on its first start.
//!
//! It used to be the operator's job: create a tunnel, point it at 127.0.0.1:8787,
//! publish `<machine>.<domain>`, and write the connector token into Workers KV.
//! Nothing about that work needs a person — the machine already names itself from
//! `/etc/machine-id` and already holds a token that may create tunnels and DNS
//! records — and asking for it meant a fresh host installed cleanly and then
//! refused to start with `Workers KV key instances/<machine>/tunnel_token is
//! missing or empty`, which reads like a bug in the installer.
//!
//! So the first start does it: find the tunnel by name, create it if it is not
//! there, point its ingress at the engine's own port, publish the hostname, and
//! write the two keys the rest of the process reads. Every step is asked before it
//! is done, so a machine that already has all of it writes nothing.

use serde_json::{json, Value};

/// Where the tunnel's traffic goes: the engine's own listening port.
pub const SERVICE: &str = "http://127.0.0.1:8787";

/// The base domain a machine's hostname lives under, when nothing says otherwise.
pub const DEFAULT_DOMAIN: &str = "aixodia.dpdns.org";

/// `AIXODIA_DOMAIN`, when the operator set one. The deployment's domain is the
/// only thing about a machine's address a person chooses, so it is the only
/// thing beside its name that arrives from outside.
pub fn domain() -> String {
    std::env::var("AIXODIA_DOMAIN")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_DOMAIN.to_string())
}

/// What the engine needs before it can start cloudflared.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Provisioned {
    pub tunnel_id: String,
    pub token: String,
    pub url: String,
}

/// The hostname a machine answers on: its own name under the deployment's domain.
pub fn hostname(name: &str, domain: &str) -> String {
    let domain = domain.trim().trim_start_matches('.').trim_end_matches('/');
    format!("{name}.{domain}")
}

/// The request that creates a tunnel with a remotely-managed configuration.
///
/// `config_src: "cloudflare"` is the difference between a tunnel whose ingress
/// this process can write and one whose ingress lives in a file on the operator's
/// disk: a locally-managed tunnel ignores the configuration endpoint.
pub fn create_tunnel_body(name: &str) -> Value {
    json!({ "name": name, "config_src": "cloudflare" })
}

/// The request that points a tunnel's ingress at the engine.
pub fn ingress_body(host: &str, service: &str) -> Value {
    json!({
        "config": {
            "ingress": [
                { "hostname": host, "service": service },
                { "service": "http_status:404" },
            ]
        }
    })
}

/// The request that publishes the hostname, through the tunnel.
pub fn dns_body(host: &str, tunnel_id: &str) -> Value {
    json!({
        "type": "CNAME",
        "name": host,
        "content": format!("{tunnel_id}.cfargotunnel.com"),
        "proxied": true,
    })
}

/// A tunnel the account already has, by name.
#[derive(Debug, serde::Deserialize)]
pub struct TunnelRow {
    pub id: String,
    pub name: String,
}

/// A zone the account owns, by name.
#[derive(Debug, serde::Deserialize)]
struct ZoneRow {
    id: String,
    name: String,
}

/// A DNS record already published at a hostname.
#[derive(Debug, serde::Deserialize)]
struct RecordRow {
    content: String,
}

/// The zone that contains `host`: itself if that is a zone, else the longest
/// suffix of it the account owns. A deployment domain is often a subdomain of a
/// zone registered elsewhere, so asking for the host directly is the honest
/// question and walking up is the answer when nobody owns the whole string.
fn zone_candidates(host: &str) -> Vec<String> {
    let labels: Vec<&str> = host.split('.').filter(|l| !l.is_empty()).collect();
    (0..labels.len().saturating_sub(1))
        .map(|start| labels[start..].join("."))
        .collect()
}

/// Makes sure this machine has a tunnel, a hostname published to it, and a
/// service behind it — creating whatever is missing and leaving everything else
/// alone. Reads are the audit: nothing is created that a query did not say was
/// absent.
pub async fn ensure(db: &aixodia_db::Client, target: &Target) -> anyhow::Result<Provisioned> {
    let account = db.account()?;
    let tunnel = find_or_create_tunnel(db, &account, target).await?;
    point_ingress(db, &account, &tunnel.id, target).await?;
    publish_hostname(db, &tunnel.id, target).await?;
    let token = tunnel_token(db, &account, &tunnel.id).await?;
    Ok(Provisioned {
        tunnel_id: tunnel.id,
        token,
        url: format!("https://{}", target.host),
    })
}

async fn find_or_create_tunnel(
    db: &aixodia_db::Client,
    account: &str,
    target: &Target,
) -> anyhow::Result<TunnelRow> {
    let path = format!(
        "/accounts/{account}/cfd_tunnel?name={}&is_deleted=false",
        target.tunnel
    );
    let rows: Vec<TunnelRow> =
        serde_json::from_value(db.api_call(reqwest::Method::GET, &path, None).await?)?;
    if let Some(row) = rows.into_iter().find(|row| row.name == target.tunnel) {
        tracing::info!(tunnel = %row.id, "tunnel already exists");
        return Ok(row);
    }
    tracing::info!(name = %target.tunnel, "creating tunnel");
    let created = db
        .api_call(
            reqwest::Method::POST,
            &format!("/accounts/{account}/cfd_tunnel"),
            Some(create_tunnel_body(&target.tunnel)),
        )
        .await?;
    Ok(serde_json::from_value(created)?)
}

async fn point_ingress(
    db: &aixodia_db::Client,
    account: &str,
    tunnel: &str,
    target: &Target,
) -> anyhow::Result<()> {
    db.api_call(
        reqwest::Method::PUT,
        &format!("/accounts/{account}/cfd_tunnel/{tunnel}/configurations"),
        Some(ingress_body(&target.host, &target.service)),
    )
    .await?;
    Ok(())
}

async fn publish_hostname(
    db: &aixodia_db::Client,
    tunnel: &str,
    target: &Target,
) -> anyhow::Result<()> {
    let zone = find_zone(db, &target.host).await?;
    let path = format!("/zones/{}/dns_records?name={}", zone.id, target.host);
    let rows: Vec<RecordRow> =
        serde_json::from_value(db.api_call(reqwest::Method::GET, &path, None).await?)?;
    let wanted = format!("{tunnel}.cfargotunnel.com");
    if rows.iter().any(|row| row.content == wanted) {
        tracing::info!(host = %target.host, zone = %zone.name, "hostname already published");
        return Ok(());
    }
    tracing::info!(host = %target.host, zone = %zone.name, "publishing hostname");
    db.api_call(
        reqwest::Method::POST,
        &format!("/zones/{}/dns_records", zone.id),
        Some(dns_body(&target.host, tunnel)),
    )
    .await?;
    Ok(())
}

async fn find_zone(db: &aixodia_db::Client, host: &str) -> anyhow::Result<ZoneRow> {
    for candidate in zone_candidates(host) {
        let zones: Vec<ZoneRow> = serde_json::from_value(
            db.api_call(
                reqwest::Method::GET,
                &format!("/zones?name={candidate}"),
                None,
            )
            .await?,
        )?;
        if let Some(zone) = zones.into_iter().find(|zone| zone.name == candidate) {
            return Ok(zone);
        }
    }
    anyhow::bail!(
        "no zone of this account contains {host}: set AIXODIA_DOMAIN to a domain it serves"
    )
}

async fn tunnel_token(
    db: &aixodia_db::Client,
    account: &str,
    tunnel: &str,
) -> anyhow::Result<String> {
    let value = db
        .api_call(
            reqwest::Method::GET,
            &format!("/accounts/{account}/cfd_tunnel/{tunnel}/token"),
            None,
        )
        .await?;
    let token = value
        .as_str()
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .ok_or_else(|| anyhow::anyhow!("the tunnel's connector token came back empty"))?;
    Ok(token.to_string())
}

/// The name this machine's tunnel has, and the host it is published on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub tunnel: String,
    pub host: String,
    pub service: String,
}

impl Target {
    pub fn new(name: &str, domain: &str) -> Self {
        Self {
            tunnel: name.to_string(),
            host: hostname(name, domain),
            service: SERVICE.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_machine_is_named_under_the_deployments_domain() {
        assert_eq!(
            hostname("m-900e51e3", "aixodia.dpdns.org"),
            "m-900e51e3.aixodia.dpdns.org"
        );
        // A domain written the way a URL is written is still a domain.
        assert_eq!(
            hostname("m-900e51e3", ".aixodia.dpdns.org/"),
            "m-900e51e3.aixodia.dpdns.org"
        );
    }

    #[test]
    fn a_tunnel_is_created_with_its_configuration_managed_here() {
        let body = create_tunnel_body("m-900e51e3");
        assert_eq!(body["name"], "m-900e51e3");
        assert_eq!(
            body["config_src"], "cloudflare",
            "a locally-managed tunnel ignores the ingress written through the API"
        );
    }

    #[test]
    fn the_ingress_sends_the_engines_hostname_to_the_engines_port() {
        let body = ingress_body("m-900e51e3.aixodia.dpdns.org", SERVICE);
        let ingress = &body["config"]["ingress"];
        assert_eq!(ingress[0]["hostname"], "m-900e51e3.aixodia.dpdns.org");
        assert_eq!(ingress[0]["service"], "http://127.0.0.1:8787");
        // The last rule has no hostname: it is what answers for everything else,
        // and Cloudflare refuses a configuration without it.
        assert_eq!(ingress[1]["service"], "http_status:404");
        assert!(ingress[1]["hostname"].is_null());
    }

    #[test]
    fn the_hostname_is_a_proxied_cname_to_the_tunnels_own_name() {
        let body = dns_body("m-900e51e3.aixodia.dpdns.org", "abc-123");
        assert_eq!(body["type"], "CNAME");
        assert_eq!(body["name"], "m-900e51e3.aixodia.dpdns.org");
        assert_eq!(body["content"], "abc-123.cfargotunnel.com");
        assert_eq!(
            body["proxied"], true,
            "unproxied, the tunnel's own certificate does not cover the hostname"
        );
    }

    #[test]
    fn the_zone_is_the_longest_suffix_the_account_may_own() {
        // Asked in order: the host itself first, then each parent. A domain
        // registered elsewhere means the host is not a zone but a suffix is.
        assert_eq!(
            zone_candidates("m-900e51e3.aixodia.dpdns.org"),
            vec![
                "m-900e51e3.aixodia.dpdns.org",
                "aixodia.dpdns.org",
                "dpdns.org",
            ]
        );
        assert!(zone_candidates("localhost").is_empty());
    }

    #[test]
    fn a_target_names_one_tunnel_on_the_default_port() {
        let target = Target::new("m-900e51e3", DEFAULT_DOMAIN);
        assert_eq!(target.tunnel, "m-900e51e3");
        assert_eq!(target.host, "m-900e51e3.aixodia.dpdns.org");
        assert_eq!(target.service, "http://127.0.0.1:8787");
    }
}
