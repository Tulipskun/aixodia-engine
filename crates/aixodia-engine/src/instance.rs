//! Which engine this is, when several machines share one public domain.
//!
//! Each machine runs its own engine on `127.0.0.1:8787`, behind its own Named
//! Tunnel. What they cannot share is the Workers KV namespace: `daemon/url` and
//! `tunnel_token` are single keys, so the second machine to start would publish
//! over the first one and both would dial the same tunnel.
//!
//! The engine therefore names itself, and names itself the same way every time
//! it starts on that machine: a short tag derived from the machine's own
//! `/etc/machine-id`. Nothing has to be configured — `CF_TOKEN` remains the only
//! variable an install needs — and two machines can never pick the same tag
//! unless they were cloned from one another. The machine id is used instead of
//! the hostname because a hostname can be reassigned (a VM is restarted with a
//! new one), which would strand the KV keys and tunnel provisioned under the
//! old name; the machine id stays with the disk.
//!
//! An operator who wants a readable name in the public URL sets
//! `AIXODIA_INSTANCE` (lower-case letters, digits, single hyphens). It is an
//! override, never a requirement.

use anyhow::anyhow;

/// The prefix every per-machine key lives under, and the path segment the
/// gateway resolves.
pub const KEY_ROOT: &str = "instances";

/// The KV keys belonging to this machine.
#[derive(Clone, Debug)]
pub struct Keys {
    name: String,
}

impl Keys {
    /// The keys of this machine, named from `AIXODIA_INSTANCE` when it is set
    /// and from the machine identity otherwise.
    pub fn for_this_machine() -> anyhow::Result<Self> {
        Ok(Self {
            name: machine_name()?,
        })
    }

    /// The name this machine is known by, in KV and in the public path.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The key this machine reads and writes for `base`.
    pub fn key(&self, base: &str) -> String {
        format!("{KEY_ROOT}/{}/{base}", self.name)
    }
}

/// The name of this machine, as it appears in a KV key and a public URL.
pub fn machine_name() -> anyhow::Result<String> {
    if let Some(override_name) = override_name()? {
        return Ok(override_name);
    }
    let tag = machine_tag()?;
    Ok(format!("m-{tag}"))
}

/// `AIXODIA_INSTANCE`, when the operator set it.
fn override_name() -> anyhow::Result<Option<String>> {
    let Some(name) = std::env::var("AIXODIA_INSTANCE")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    else {
        return Ok(None);
    };
    if !aixodia_state::valid_instance_name(&name) {
        return Err(anyhow!(
            "AIXODIA_INSTANCE must be lower-case letters, digits and single hyphens, 1-32 characters: {name}"
        ));
    }
    Ok(Some(name))
}

/// Eight hex digits of the machine id: stable across reboots, boots of the same
/// disk, engine updates, and different for every machine.
///
/// The machine id is a uuid; hashing it keeps the name from looking like one and
/// keeps it inside the 32-character path segment the gateway accepts.
fn machine_tag() -> anyhow::Result<String> {
    if let Some(mid) = read_first_line("/etc/machine-id") {
        use sha2::Digest as _;
        let digest = sha2::Sha256::digest(mid.as_bytes());
        let hex = digest
            .iter()
            .take(4)
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        return Ok(hex);
    }
    // No machine id: a host that never had one (some chroots) still needs a
    // name, and the kernel's hostname is the only stable thing left. It is hashed
    // for the same reason as above and because a hostname is not a path segment.
    if let Some(host) = read_first_line("/proc/sys/kernel/hostname") {
        if !host.is_empty() {
            use sha2::Digest as _;
            let digest = sha2::Sha256::digest(host.as_bytes());
            let hex = digest
                .iter()
                .take(4)
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            return Ok(hex);
        }
    }
    Err(anyhow!(
        "this machine has no /etc/machine-id and no hostname to name itself by; set AIXODIA_INSTANCE"
    ))
}

fn read_first_line(path: &str) -> Option<String> {
    let raw = std::fs::read_to_string(path).ok()?;
    let line = raw.lines().next().unwrap_or_default().trim().to_string();
    (!line.is_empty()).then_some(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_key_is_this_machines_own() {
        let keys = Keys {
            name: "m-8a7d1192".to_string(),
        };
        assert_eq!(
            keys.key("tunnel_token"),
            "instances/m-8a7d1192/tunnel_token"
        );
        assert_eq!(keys.key("daemon/url"), "instances/m-8a7d1192/daemon/url");
        assert_eq!(keys.name(), "m-8a7d1192");
        assert_eq!(keys.key("tunnel_url"), "instances/m-8a7d1192/tunnel_url");
    }

    #[test]
    fn a_name_is_a_path_segment_and_never_changes_between_lookups() {
        std::env::remove_var("AIXODIA_INSTANCE");
        let name = machine_name().expect("this machine can name itself");
        assert!(aixodia_state::valid_instance_name(&name), "{name}");
        assert_eq!(
            name,
            machine_name().expect("stable"),
            "the name must be stable"
        );
        if name.starts_with("m-") {
            assert_eq!(name.len(), 10, "eight hex digits after the prefix: {name}");
            assert!(name[2..].chars().all(|c| c.is_ascii_hexdigit()), "{name}");
        }
    }

    #[test]
    fn the_name_follows_the_machine_id_not_the_hostname() {
        std::env::remove_var("AIXODIA_INSTANCE");
        let Some(mid) = read_first_line("/etc/machine-id") else {
            return; // a host with no machine id falls back to the hostname
        };
        use sha2::Digest as _;
        let digest = sha2::Sha256::digest(mid.as_bytes());
        let expected = format!(
            "m-{}",
            digest
                .iter()
                .take(4)
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        );
        assert_eq!(
            machine_name().unwrap(),
            expected,
            "the name is derived from /etc/machine-id, not a reassignable hostname"
        );
    }

    #[test]
    fn the_override_names_the_machine_and_is_validated() {
        let saved = std::env::var_os("AIXODIA_INSTANCE");
        std::env::set_var("AIXODIA_INSTANCE", "phone-2");
        assert_eq!(machine_name().unwrap(), "phone-2");
        std::env::set_var("AIXODIA_INSTANCE", "Phone_2");
        assert!(
            machine_name().is_err(),
            "a name that could break a key or a URL is refused, not escaped"
        );
        match saved {
            Some(value) => std::env::set_var("AIXODIA_INSTANCE", value),
            None => std::env::remove_var("AIXODIA_INSTANCE"),
        }
    }
}
