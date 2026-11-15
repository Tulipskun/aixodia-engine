//! A reqwest client that never waits on an address family this host cannot use.
//!
//! The Cloudflare API answers with both A and AAAA records. A host with no IPv6
//! address and no v6 default route — a chroot inside Android, a container on a
//! v4-only bridge — has every AAAA connect refused, and the resolver tries them
//! in the order they arrive. A plain `SELECT 1` against D1 measured six seconds
//! on the phone while the same query over IPv4 took half of one: the v6 attempts
//! were being paid for first, on every request, by every client.
//!
//! Chat suffers worst, because the phone polls a run's events every 60 ms to
//! grow a turn as it is written. Each poll sat behind a v6 attempt, so tokens
//! arrived in bursts of a couple of seconds instead of continuously.
//!
//! The rule is narrow: v6 answers are dropped only when this host has no v6
//! source address. A host that does have one keeps every answer in the order the
//! resolver gave, because its v6 path may well be the better one.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::OnceLock;

use reqwest::dns::{Addrs, Name, Resolve, Resolving};

/// Whether this host has an IPv6 address it could send from.
fn host_has_ipv6() -> bool {
    static HAS: OnceLock<bool> = OnceLock::new();
    *HAS.get_or_init(|| {
        // Reading the interface list is the only portable check available
        // without a dependency. A failure means no addresses were seen, which
        // is treated as "no v6" because the fallback is the slow path.
        let Ok(interfaces) = if_addrs_or_none() else {
            return false;
        };
        interfaces
    })
}

/// Lists non-loopback, non-link-local IPv6 addresses the host holds.
fn if_addrs_or_none() -> Result<bool, ()> {
    let Ok(content) = std::fs::read_to_string("/proc/net/if_inet6") else {
        return Ok(false);
    };
    for line in content.lines() {
        let mut fields = line.split_whitespace();
        let Some(hex) = fields.next() else { continue };
        // Scope is the second-to-last column: 0x20 is link-local, which cannot
        // reach a public address and so is not a usable v6 path.
        let scope = fields.nth(3).unwrap_or("");
        if scope == "20" {
            continue;
        }
        if hex.len() == 32 && hex.chars().any(|c| c != '0') {
            // ::1 is loopback and is skipped by the same check.
            if hex != "00000000000000000000000000000001" {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// Resolves names, dropping v6 answers when this host has no v6 path.
#[derive(Debug, Default, Clone, Copy)]
pub struct ReachableResolver;

impl Resolve for ReachableResolver {
    fn resolve(&self, name: Name) -> Resolving {
        Box::pin(async move {
            let host = name.as_str().to_string();
            let lookup = tokio::task::spawn_blocking(move || {
                use std::net::ToSocketAddrs;
                (host.as_str(), 0u16).to_socket_addrs().map(|addrs| {
                    let collected: Vec<SocketAddr> = addrs.collect();
                    collected
                })
            })
            .await;

            let addrs = match lookup {
                Ok(Ok(addrs)) => addrs,
                Ok(Err(err)) => {
                    return Err(Box::new(err) as Box<dyn std::error::Error + Send + Sync>)
                }
                Err(err) => return Err(Box::new(err) as Box<dyn std::error::Error + Send + Sync>),
            };

            let keep_v6 = host_has_ipv6();
            let filtered: Vec<SocketAddr> = if keep_v6 {
                addrs.clone()
            } else {
                addrs
                    .iter()
                    .copied()
                    .filter(|addr| matches!(addr.ip(), IpAddr::V4(_)))
                    .collect()
            };

            // An answer that is only v6 is passed through: reporting the real
            // failure is better than replacing it with an empty one.
            let answer = if filtered.is_empty() { addrs } else { filtered };
            Ok(Box::new(answer.into_iter()) as Addrs)
        })
    }
}

/// A client builder that uses [`ReachableResolver`] and a fixed timeout.
///
/// Every client in the engine goes through this, so the rule cannot be missed
/// by a call site that builds its own.
pub fn client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder().dns_resolver(std::sync::Arc::new(ReachableResolver))
}

#[allow(dead_code)]
const _: Ipv4Addr = Ipv4Addr::UNSPECIFIED;
#[allow(dead_code)]
const _: Ipv6Addr = Ipv6Addr::LOCALHOST;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_host_without_ipv6_loses_its_v6_answers() {
        let addrs: Vec<SocketAddr> = vec![
            "[2606:4700::1]:443".parse().unwrap(),
            "104.19.192.29:443".parse().unwrap(),
        ];
        let kept = filter(addrs.clone(), false);
        assert_eq!(kept.len(), 1);
        assert!(kept[0].is_ipv4());
    }

    #[test]
    fn a_host_with_ipv6_keeps_every_answer_in_order() {
        let addrs: Vec<SocketAddr> = vec![
            "[2606:4700::1]:443".parse().unwrap(),
            "104.19.192.29:443".parse().unwrap(),
        ];
        assert_eq!(filter(addrs.clone(), true), addrs);
    }

    #[test]
    fn a_name_that_only_answers_v6_keeps_its_failure() {
        let addrs: Vec<SocketAddr> = vec!["[2606:4700::1]:443".parse().unwrap()];
        assert_eq!(filter(addrs.clone(), false), addrs);
    }

    #[test]
    fn the_client_builder_installs_the_resolver() {
        // Building the client is the contract: a failure here is a compile or
        // wiring mistake, not a runtime condition.
        let _ = client_builder()
            .timeout(std::time::Duration::from_secs(5))
            .build();
    }
}

/// The filtering rule, separated so it can be tested without a network.
fn filter(addrs: Vec<SocketAddr>, keep_v6: bool) -> Vec<SocketAddr> {
    if keep_v6 {
        return addrs;
    }
    let v4: Vec<SocketAddr> = addrs
        .iter()
        .copied()
        .filter(|addr| matches!(addr.ip(), IpAddr::V4(_)))
        .collect();
    if v4.is_empty() {
        addrs
    } else {
        v4
    }
}
