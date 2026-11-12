//! The adapter registry: every adapter name a provider row may use.
//!
//! Port of the Go `provider/registry` package.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::anthropic;
use crate::openai;
use crate::opencode;
use crate::types::Adapter;

/// Maps the `adapter` field of a provider row to an implementation.
pub struct Registry {
    adapters: BTreeMap<String, Arc<dyn Adapter>>,
}

impl Registry {
    pub fn new() -> Self {
        let openai: Arc<dyn Adapter> = Arc::new(openai::Adapter::new());
        let mut adapters: BTreeMap<String, Arc<dyn Adapter>> = BTreeMap::new();
        adapters.insert("openai".to_string(), openai.clone());
        // Gemini exposes an OpenAI-compatible endpoint, so the openai adapter
        // serves it with the provider's endpoint set to that base URL.
        adapters.insert("gemini".to_string(), openai);
        adapters.insert("anthropic".to_string(), Arc::new(anthropic::Adapter::new()));
        adapters.insert("opencode".to_string(), Arc::new(opencode::Adapter::new()));
        Self { adapters }
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Adapter>> {
        self.adapters.get(name).cloned()
    }

    pub fn names(&self) -> Vec<String> {
        self.adapters.keys().cloned().collect()
    }

    pub fn require(&self, name: &str) -> anyhow::Result<Arc<dyn Adapter>> {
        self.get(name).ok_or_else(|| {
            anyhow::anyhow!(
                "unknown adapter {:?} (available: {})",
                name,
                self.names().join(", ")
            )
        })
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_known_adapter_resolves() {
        let registry = Registry::new();
        assert_eq!(
            registry.names(),
            vec!["anthropic", "gemini", "openai", "opencode"]
        );
        for name in registry.names() {
            let adapter = registry.require(&name).expect("adapter");
            if name == "gemini" {
                // Gemini is served by the openai adapter.
                assert_eq!(adapter.name(), "openai");
            } else {
                assert_eq!(adapter.name(), name);
            }
        }
        let err = match registry.require("nope") {
            Ok(_) => panic!("unknown adapter should fail"),
            Err(err) => err.to_string(),
        };
        assert!(err.contains("unknown adapter"));
    }
}
