//! Which engine this process is, when more than one shares a host.
//!
//! Two engines on the same machine must not answer on the same port, must not
//! read each other's tunnel credentials, and must not publish to the same
//! Workers KV keys — the last one is what makes a single public domain possible:
//! every instance gets its own key prefix, and the gateway in front resolves the
//! first path segment to the instance's own tunnel hostname.
//!
//! Nothing here is required. With neither variable set the engine answers on
//! `127.0.0.1:8787` and reads the keys it always read, so a host that runs one
//! engine is unaffected by the feature existing.

use anyhow::anyhow;

/// The address the engine answers on when nothing says otherwise.
pub const DEFAULT_BIND: &str = "127.0.0.1:8787";

/// The address this engine answers on, from `AIXODIA_BIND`.
pub fn bind() -> anyhow::Result<String> {
    let bind = std::env::var("AIXODIA_BIND")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_BIND.to_string());
    let parsed: std::net::SocketAddr = bind
        .parse()
        .map_err(|_| anyhow!("AIXODIA_BIND is not an address and port: {bind}"))?;
    Ok(parsed.to_string())
}

/// The Workers KV keys belonging to this engine.
///
/// A named instance owns `instances/<name>/…`; an engine started without a name
/// owns the bare keys, which is exactly what an install did before instances
/// existed.
#[derive(Clone, Debug)]
pub struct Keys {
    instance: Option<String>,
}

impl Keys {
    /// Reads `AIXODIA_INSTANCE`, rejecting a name that is not a path segment.
    pub fn from_env() -> anyhow::Result<Self> {
        let raw = std::env::var("AIXODIA_INSTANCE")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        let Some(instance) = raw else {
            return Ok(Self { instance: None });
        };
        if !aixodia_state::valid_instance_name(&instance) {
            return Err(anyhow!(
                "AIXODIA_INSTANCE must be lower-case letters, digits and single hyphens, 1-32 characters: {instance}"
            ));
        }
        Ok(Self {
            instance: Some(instance),
        })
    }

    /// The instance name, when this engine has one.
    pub fn instance(&self) -> Option<&str> {
        self.instance.as_deref()
    }

    /// The key this instance reads and writes for `base`.
    pub fn key(&self, base: &str) -> String {
        match &self.instance {
            Some(instance) => format!("instances/{instance}/{base}"),
            None => base.to_string(),
        }
    }

    /// The prefix a gateway lists this instance's keys under, empty when the
    /// engine is the unnamed one.
    pub fn prefix(&self) -> String {
        match &self.instance {
            Some(instance) => format!("instances/{instance}/"),
            None => String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(instance: Option<&str>) -> Keys {
        Keys {
            instance: instance.map(str::to_string),
        }
    }

    #[test]
    fn an_unnamed_engine_keeps_the_keys_it_always_had() {
        let keys = keys(None);
        assert_eq!(keys.key("tunnel_token"), "tunnel_token");
        assert_eq!(keys.key("daemon/url"), "daemon/url");
        assert_eq!(keys.prefix(), "");
    }

    #[test]
    fn a_named_engine_owns_its_own_keys() {
        let keys = keys(Some("phone-2"));
        assert_eq!(keys.key("tunnel_token"), "instances/phone-2/tunnel_token");
        assert_eq!(keys.key("daemon/url"), "instances/phone-2/daemon/url");
        assert_eq!(keys.prefix(), "instances/phone-2/");
        assert_eq!(keys.instance(), Some("phone-2"));
    }

    #[test]
    fn the_bind_address_defaults_and_is_validated() {
        let saved = std::env::var_os("AIXODIA_BIND");
        std::env::remove_var("AIXODIA_BIND");
        assert_eq!(bind().expect("the default bind is valid"), DEFAULT_BIND);
        std::env::set_var("AIXODIA_BIND", "127.0.0.1:8788");
        assert_eq!(bind().unwrap(), "127.0.0.1:8788");
        std::env::set_var("AIXODIA_BIND", "not-an-address");
        assert!(bind().is_err(), "a broken bind must not be guessed at");
        match saved {
            Some(value) => std::env::set_var("AIXODIA_BIND", value),
            None => std::env::remove_var("AIXODIA_BIND"),
        }
    }

    #[test]
    fn an_instance_name_that_could_escape_its_key_is_rejected() {
        let saved = std::env::var_os("AIXODIA_INSTANCE");
        std::env::set_var("AIXODIA_INSTANCE", "..%2f");
        assert!(Keys::from_env().is_err());
        std::env::set_var("AIXODIA_INSTANCE", "main");
        assert_eq!(Keys::from_env().unwrap().instance(), Some("main"));
        match saved {
            Some(value) => std::env::set_var("AIXODIA_INSTANCE", value),
            None => std::env::remove_var("AIXODIA_INSTANCE"),
        }
    }
}
