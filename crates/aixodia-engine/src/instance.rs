//! Which engine this is, when several machines share one public domain.
//!
//! Each machine runs its own engine on `127.0.0.1:8787`, behind its own Named
//! Tunnel. What they cannot share is the Workers KV namespace: `daemon/url` and
//! `tunnel_token` are single keys, so the second machine to start would publish
//! over the first one and both would dial the same tunnel.
//!
//! The engine therefore names itself, and names itself the same way every time
//! it starts on that machine: the device's own name, the hostname it answers to,
//! reduced to what a path segment may carry. Nothing has to be configured —
//! `CF_TOKEN` remains the only variable an install needs — and two machines that
//! answer to the same name are told apart by the registry's suffix (`-1`, `-2`),
//! never by a hash of their machine id: a name a person can read is worth more
//! than a name that cannot collide.
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
    device_name()
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

/// The device's own name: the short hostname this machine answers to, in the
/// alphabet a path segment and a KV key allow.
///
/// The same machine names itself the same thing every boot, and a person reading
/// the URL recognises the machine. Two devices that were given one name are not
/// this function's problem — the registry hands out `-1`, `-2` and so on.
fn device_name() -> anyhow::Result<String> {
    for path in ["/proc/sys/kernel/hostname", "/etc/hostname"] {
        if let Some(name) = read_first_line(path).as_deref().and_then(sanitize_name) {
            return Ok(name);
        }
    }
    Err(anyhow!(
        "this machine has no hostname to name itself by; set AIXODIA_INSTANCE"
    ))
}

/// A hostname reduced to a name: lower case, one hyphen where a separator was,
/// no hyphen at either end, at most thirty-two characters, and only the part
/// before the first dot — a device is `work`, not `work.example.lan`.
fn sanitize_name(raw: &str) -> Option<String> {
    let short = raw.trim().split('.').next().unwrap_or_default();
    let mut name = String::with_capacity(short.len());
    let mut separator = false;
    for character in short.chars() {
        let lowered = character.to_ascii_lowercase();
        if lowered.is_ascii_alphanumeric() {
            name.push(lowered);
            separator = false;
        } else if !name.is_empty() && !separator {
            name.push('-');
            separator = true;
        }
    }
    let name = name.trim_matches('-');
    let name: String = name.chars().take(32).collect();
    let name = name.trim_end_matches('-');
    (!name.is_empty() && aixodia_state::valid_instance_name(name)).then(|| name.to_string())
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
            name: "work".to_string(),
        };
        assert_eq!(keys.key("tunnel_token"), "instances/work/tunnel_token");
        assert_eq!(keys.key("daemon/url"), "instances/work/daemon/url");
        assert_eq!(keys.name(), "work");
        assert_eq!(keys.key("tunnel_url"), "instances/work/tunnel_url");
    }

    #[test]
    fn the_name_is_the_device_name_in_the_alphabet_a_url_allows() {
        assert_eq!(sanitize_name("work").as_deref(), Some("work"));
        assert_eq!(sanitize_name("Work-Laptop").as_deref(), Some("work-laptop"));
        assert_eq!(sanitize_name("work.example.lan").as_deref(), Some("work"));
        assert_eq!(
            sanitize_name("  studio mac  ").as_deref(),
            Some("studio-mac")
        );
        assert_eq!(sanitize_name("a__b--c").as_deref(), Some("a-b-c"));
        assert_eq!(sanitize_name("---"), None);
        assert_eq!(sanitize_name(""), None);
        let long = sanitize_name(&"a".repeat(60)).expect("still a name");
        assert_eq!(long.len(), 32);
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
        let host = super::read_first_line("/proc/sys/kernel/hostname")
            .and_then(|raw| sanitize_name(&raw))
            .expect("the kernel names this machine");
        assert_eq!(name, host, "the name is the device's, not a hash of it");
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
