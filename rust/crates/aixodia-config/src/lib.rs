//! The provider store.
//!
//! Providers and the API keys that exist nowhere else live in Workers KV, one
//! key holding the whole list. Adding or removing a provider is one atomic
//! read-modify-write, and a config reload costs one KV read.
//!
//! Port of the engine's Go `config` package.

use std::sync::{Arc, OnceLock, RwLock};

use aixodia_db::KeyValue;
use serde::{Deserialize, Serialize};

/// Errors from the provider store.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("config: the provider store is not open")]
    StoreNotOpen,
    #[error("config: load providers: {0}")]
    Load(#[source] Box<Error>),
    #[error("config: providers in KV are not valid JSON: {0}")]
    Corrupt(#[source] serde_json::Error),
    #[error("config: {name} is a built-in provider and cannot be {action}")]
    Builtin { name: String, action: &'static str },
    #[error(transparent)]
    Db(#[from] aixodia_db::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

/// One provider as the rest of the engine sees it. `id` is only an API label;
/// providers are looked up by name.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Provider {
    pub id: String,
    pub name: String,
    pub api_url: String,
    /// Where the model catalog lives when it is not under `api_url`; empty
    /// means the catalog is asked at `api_url` itself.
    pub models_url: String,
    pub keys: Vec<String>,
    pub adapter: String,
}

/// The providers the engine carries itself, so the phone has something to talk
/// to before — or without — any provider being added. Neither of them needs an
/// API key, which is what makes them safe to ship turned on and why they are not
/// stored in KV. They come first, so a fresh install answers on the keyless one.
fn builtin_providers() -> Vec<Provider> {
    vec![
        // Non-login mode proper: a public endpoint that answers an unauthenticated
        // request. Its anonymous tier is throttled per IP, which the app reports
        // rather than hides.
        Provider {
            name: "Pollinations".to_string(),
            api_url: "https://text.pollinations.ai/openai".to_string(),
            adapter: "openai".to_string(),
            ..Provider::default()
        },
        // The OpenCode Zen tier. Its free models are served only to requests
        // that match the official client's contract; the opencode adapter
        // presents that contract (TLS fingerprint, anonymous token, request
        // shape), so the free models answer without the CLI installed.
        Provider {
            name: "Opencode".to_string(),
            api_url: "https://opencode.ai/inference/openai/v1".to_string(),
            // Zen serves chat under …/openai/v1 but its catalog one level up.
            models_url: "https://opencode.ai/inference/v1".to_string(),
            adapter: "opencode".to_string(),
            ..Provider::default()
        },
    ]
}

/// The providers currently served, KV entries first then the built-ins.
fn providers_slot() -> &'static RwLock<Vec<Provider>> {
    static PROVIDERS: OnceLock<RwLock<Vec<Provider>>> = OnceLock::new();
    PROVIDERS.get_or_init(|| RwLock::new(Vec::new()))
}

/// Whether `name` belongs to a provider the engine carries itself. Built-ins are
/// always listed and can never be added, edited or removed through the API.
pub fn is_builtin(name: &str) -> bool {
    builtin_providers()
        .iter()
        .any(|provider| provider.name.eq_ignore_ascii_case(name))
}

/// A copy of the provider list, in stored order (which is the order the phone
/// sees).
pub fn get_providers() -> Vec<Provider> {
    providers_slot().read().unwrap().clone()
}

/// Looks a provider up by its API label.
pub fn get_provider(id: &str) -> Option<Provider> {
    providers_slot()
        .read()
        .unwrap()
        .iter()
        .find(|provider| provider.id == id)
        .cloned()
}

/// Looks a provider up by name, case-insensitively.
pub fn get_provider_by_name(name: &str) -> Option<Provider> {
    providers_slot()
        .read()
        .unwrap()
        .iter()
        .find(|provider| provider.name.eq_ignore_ascii_case(name))
        .cloned()
}

// --- stored shape --------------------------------------------------------

/// The one key the store reads and writes.
const PROVIDERS_KEY: &str = "providers";

/// The shape of one provider in the stored JSON. Field names mirror the old D1
/// columns (`provider`, `endpoint`, `adapter`, `keys`) so the value stays
/// recognisable to anything that read that table.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProviderEntry {
    #[serde(default)]
    id: i64,
    #[serde(default)]
    provider: String,
    #[serde(default)]
    endpoint: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    models_url: String,
    #[serde(default)]
    keys: Vec<String>,
    #[serde(default)]
    adapter: String,
}

/// The value stored under [`PROVIDERS_KEY`].
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProviderFile {
    #[serde(default)]
    next_id: i64,
    #[serde(default)]
    providers: Vec<ProviderEntry>,
}

/// Keeps a provider with no key as an empty list rather than null, so the stored
/// JSON always has an array a reader can iterate.
fn non_nil_keys(keys: Vec<String>) -> Vec<String> {
    keys
}

/// Providers backed by a KV namespace.
pub struct Store {
    kv: Arc<dyn KeyValue>,
}

impl Store {
    pub fn new(kv: Arc<dyn KeyValue>) -> Self {
        Self { kv }
    }

    /// The stored file. A key that was never written is an empty file, not an
    /// error: a fresh namespace has no providers yet.
    async fn read(&self) -> Result<ProviderFile> {
        let raw = self.kv.get(PROVIDERS_KEY).await?;
        let Some(raw) = raw.filter(|raw| !raw.trim().is_empty()) else {
            return Ok(ProviderFile {
                next_id: 1,
                providers: Vec::new(),
            });
        };

        let mut file: ProviderFile = serde_json::from_str(&raw).map_err(Error::Corrupt)?;

        // A file written before next_id existed, or one whose counter fell
        // behind, must not hand out an ID twice.
        let mut lowest = 1;
        for entry in &file.providers {
            if entry.id >= lowest {
                lowest = entry.id + 1;
            }
        }
        if file.next_id < lowest {
            file.next_id = lowest;
        }
        Ok(file)
    }

    async fn write(&self, file: ProviderFile) -> Result<()> {
        let encoded = serde_json::to_string(&file).expect("provider file serialises");
        self.kv.put(PROVIDERS_KEY, &encoded).await?;
        Ok(())
    }

    /// The providers in stored order.
    pub async fn list(&self) -> Result<Vec<Provider>> {
        let file = self.read().await?;
        Ok(file
            .providers
            .into_iter()
            .map(|entry| Provider {
                id: entry.id.to_string(),
                name: entry.provider,
                api_url: entry.endpoint,
                models_url: entry.models_url,
                keys: entry.keys,
                adapter: entry.adapter,
            })
            .collect())
    }

    /// Writes a provider and assigns its ID.
    pub async fn add(&self, provider: Provider) -> Result<()> {
        let mut file = self.read().await?;
        let id = file.next_id.max(1);
        file.providers.push(ProviderEntry {
            id,
            provider: provider.name,
            endpoint: provider.api_url,
            models_url: provider.models_url,
            keys: non_nil_keys(provider.keys),
            adapter: provider.adapter,
        });
        file.next_id = id + 1;
        self.write(file).await
    }

    /// Removes every provider with this name, case-insensitively.
    pub async fn remove(&self, name: &str) -> Result<()> {
        let mut file = self.read().await?;
        file.providers
            .retain(|entry| !entry.provider.eq_ignore_ascii_case(name));
        self.write(file).await
    }

    /// Replaces the keys of every provider with this name.
    pub async fn set_keys(&self, name: &str, keys: Vec<String>) -> Result<()> {
        let mut file = self.read().await?;
        for entry in &mut file.providers {
            if entry.provider.eq_ignore_ascii_case(name) {
                entry.keys = non_nil_keys(keys.clone());
            }
        }
        self.write(file).await
    }
}

// --- the package-level store ---------------------------------------------

fn store_slot() -> &'static RwLock<Option<Arc<Store>>> {
    static STORE: OnceLock<RwLock<Option<Arc<Store>>>> = OnceLock::new();
    STORE.get_or_init(|| RwLock::new(None))
}

/// Points the package at the store opened at boot. Providers are read from it on
/// a timer and written by the API, so it is set once and never swapped.
pub fn set_store(store: Option<Arc<Store>>) {
    *store_slot().write().unwrap() = store;
}

fn current() -> Result<Arc<Store>> {
    store_slot()
        .read()
        .unwrap()
        .clone()
        .ok_or(Error::StoreNotOpen)
}

/// Reads the providers into memory. They live in KV, so this is the one place
/// the engine reads them; the built-ins are carried by the engine and appended
/// after them, so a KV provider with the same name wins the lookup.
pub async fn load_config() -> Result<()> {
    let store = current()?;
    let stored = match store.list().await {
        Ok(stored) => stored,
        Err(err) => {
            // The built-ins do not depend on KV. Publish them even when the
            // read fails so a boot without KV still has the free provider, and
            // keep the last good list instead of dropping it.
            let mut providers = providers_slot().write().unwrap();
            if providers.is_empty() {
                *providers = builtin_providers();
            }
            return Err(Error::Load(Box::new(err)));
        }
    };

    let builtin_count = builtin_providers().len();
    let mut providers = stored;
    providers.extend(builtin_providers());
    let total = providers.len();
    *providers_slot().write().unwrap() = providers;

    tracing::info!(total, builtin = builtin_count, "config: loaded providers");
    Ok(())
}

/// Writes a provider for the API. The caller reloads the list afterwards and
/// reads the new provider back, so the ID is never guessed here.
pub async fn add_provider(
    name: &str,
    endpoint: &str,
    adapter: &str,
    keys: Vec<String>,
) -> Result<()> {
    if is_builtin(name) {
        return Err(Error::Builtin {
            name: name.to_string(),
            action: "added",
        });
    }
    let store = current()?;
    store
        .add(Provider {
            id: String::new(),
            name: name.to_string(),
            api_url: endpoint.trim().to_string(),
            models_url: String::new(),
            keys,
            adapter: adapter.to_string(),
        })
        .await
}

pub async fn remove_provider(name: &str) -> Result<()> {
    if is_builtin(name) {
        return Err(Error::Builtin {
            name: name.to_string(),
            action: "removed",
        });
    }
    current()?.remove(name).await
}

pub async fn set_provider_keys(name: &str, keys: Vec<String>) -> Result<()> {
    if is_builtin(name) {
        return Err(Error::Builtin {
            name: name.to_string(),
            action: "changed",
        });
    }
    current()?.set_keys(name, keys).await
}

/// Clears the in-memory list. Used by tests and by a fresh boot; production code
/// only ever grows it through [`load_config`].
pub fn reset_for_tests() {
    *store_slot().write().unwrap() = None;
    providers_slot().write().unwrap().clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// The store is a package global, so tests that touch it must not overlap.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    /// The KV namespace the store talks to, kept in a map. A key that was never
    /// written is reported as missing, exactly as Workers KV does.
    struct FakeKv {
        values: RwLock<HashMap<String, String>>,
        fail: RwLock<Option<String>>,
        puts: RwLock<usize>,
    }

    impl FakeKv {
        fn new() -> Self {
            Self {
                values: RwLock::new(HashMap::new()),
                fail: RwLock::new(None),
                puts: RwLock::new(0),
            }
        }

        fn fail_with(&self, message: &str) {
            *self.fail.write().unwrap() = Some(message.to_string());
        }

        fn raw(&self, key: &str) -> Option<String> {
            self.values.read().unwrap().get(key).cloned()
        }

        fn set_raw(&self, key: &str, value: &str) {
            self.values
                .write()
                .unwrap()
                .insert(key.to_string(), value.to_string());
        }
    }

    #[async_trait::async_trait]
    impl KeyValue for FakeKv {
        async fn get(&self, key: &str) -> aixodia_db::Result<Option<String>> {
            if let Some(message) = self.fail.read().unwrap().clone() {
                return Err(fail_error(&message));
            }
            Ok(self.values.read().unwrap().get(key).cloned())
        }

        async fn put(&self, key: &str, value: &str) -> aixodia_db::Result<()> {
            if let Some(message) = self.fail.read().unwrap().clone() {
                return Err(fail_error(&message));
            }
            *self.puts.write().unwrap() += 1;
            self.values
                .write()
                .unwrap()
                .insert(key.to_string(), value.to_string());
            Ok(())
        }
    }

    fn fail_error(message: &str) -> aixodia_db::Error {
        aixodia_db::Error::Api {
            operation: "KV read".to_string(),
            message: message.to_string(),
        }
    }

    fn test_store() -> (Arc<Store>, Arc<FakeKv>) {
        let fake = Arc::new(FakeKv::new());
        let store = Arc::new(Store::new(fake.clone() as Arc<dyn KeyValue>));
        set_store(Some(store.clone()));
        providers_slot().write().unwrap().clear();
        (store, fake)
    }

    fn cleanup() {
        set_store(None);
        providers_slot().write().unwrap().clear();
    }

    fn run_test(body: impl FnOnce()) {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        body();
        cleanup();
    }

    #[test]
    fn store_round_trip() {
        run_test(|| {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let (store, fake) = test_store();
                store
                    .add(Provider {
                        id: String::new(),
                        name: "Alpha".into(),
                        api_url: "https://alpha.test".into(),
                        adapter: "openai".into(),
                        keys: vec!["k1".into(), "k2".into()],
                        ..Provider::default()
                    })
                    .await
                    .unwrap();

                let got = store.list().await.unwrap();
                assert_eq!(got.len(), 1);
                assert_eq!(got[0].id, "1");
                assert_eq!(got[0].name, "Alpha");
                assert_eq!(got[0].api_url, "https://alpha.test");
                assert_eq!(got[0].adapter, "openai");
                assert_eq!(got[0].keys, vec!["k1", "k2"]);

                // The value is one JSON object under the providers key, the
                // shape the rest of the engine expects.
                let raw = fake.raw(PROVIDERS_KEY).expect("stored value");
                let file: ProviderFile = serde_json::from_str(&raw).unwrap();
                assert_eq!(file.next_id, 2);
                assert_eq!(file.providers.len(), 1);
                assert_eq!(file.providers[0].provider, "Alpha");

                store.remove("Alpha").await.unwrap();
                assert!(store.list().await.unwrap().is_empty());
            });
        });
    }

    #[test]
    fn store_set_keys_replaces_them() {
        run_test(|| {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let (store, _) = test_store();
                store
                    .add(Provider {
                        name: "Alpha".into(),
                        api_url: "https://alpha.test".into(),
                        adapter: "openai".into(),
                        keys: vec!["old".into()],
                        ..Provider::default()
                    })
                    .await
                    .unwrap();
                store
                    .set_keys("Alpha", vec!["new".into(), "second".into()])
                    .await
                    .unwrap();
                let got = store.list().await.unwrap();
                assert_eq!(got.len(), 1);
                assert_eq!(got[0].keys, vec!["new", "second"]);
            });
        });
    }

    // A fresh namespace has no providers key at all, which is an empty list and
    // not a failure: the engine must boot on the built-in provider.
    #[test]
    fn store_with_no_key_is_empty() {
        run_test(|| {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let (store, _) = test_store();
                assert!(store.list().await.unwrap().is_empty());
                store
                    .add(Provider {
                        name: "Alpha".into(),
                        api_url: "https://alpha.test".into(),
                        adapter: "openai".into(),
                        ..Provider::default()
                    })
                    .await
                    .unwrap();
                let got = store.list().await.unwrap();
                assert_eq!(got.len(), 1);
                assert_eq!(got[0].id, "1");
            });
        });
    }

    // The engine carries its own providers, so a phone with no KV providers
    // still has something to talk to — and they are read-only.
    #[test]
    fn load_config_always_adds_the_builtin() {
        run_test(|| {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let (store, _) = test_store();
                store
                    .add(Provider {
                        name: "Alpha".into(),
                        api_url: "https://alpha.test".into(),
                        adapter: "openai".into(),
                        keys: vec!["k".into()],
                        ..Provider::default()
                    })
                    .await
                    .unwrap();
                load_config().await.unwrap();
                let got = get_providers();
                assert_eq!(got.len(), 3);
                assert_eq!(got[0].name, "Alpha");
                assert_eq!(got[1].name, "Pollinations");
                assert_eq!(got[1].keys.len(), 0);
                assert_eq!(got[2].name, "Opencode");
                assert_eq!(got[2].models_url, "https://opencode.ai/inference/v1");
                assert!(get_provider_by_name("Opencode").is_some());
            });
        });
    }

    #[test]
    fn builtin_provider_cannot_be_changed() {
        run_test(|| {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                test_store();
                assert!(is_builtin("Opencode"));
                assert!(is_builtin("opencode"));
                assert!(is_builtin("Pollinations"));
                assert!(is_builtin("pollinations"));
                assert!(add_provider(
                    "Opencode",
                    "https://elsewhere.test",
                    "openai",
                    vec!["k".into()]
                )
                .await
                .is_err());
                assert!(remove_provider("Opencode").await.is_err());
                assert!(remove_provider("Pollinations").await.is_err());
                assert!(set_provider_keys("Opencode", vec!["k".into()])
                    .await
                    .is_err());
            });
        });
    }

    #[test]
    fn store_reports_kv_failures() {
        run_test(|| {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let (store, fake) = test_store();
                fake.fail_with("db: KV read failed: HTTP 502");
                assert!(store.list().await.is_err());
                assert!(store
                    .add(Provider {
                        name: "Alpha".into(),
                        api_url: "https://alpha.test".into(),
                        adapter: "openai".into(),
                        ..Provider::default()
                    })
                    .await
                    .is_err());
            });
        });
    }

    // A boot that cannot reach KV still has the keyless providers: they are
    // carried by the engine, not by the store.
    #[test]
    fn load_config_keeps_the_builtin_when_kv_fails() {
        run_test(|| {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let (_, fake) = test_store();
                fake.fail_with("db: KV read failed: HTTP 502");
                assert!(load_config().await.is_err());
                let got = get_providers();
                assert_eq!(got.len(), 2);
                assert_eq!(got[0].name, "Pollinations");
            });
        });
    }

    #[test]
    fn store_rejects_corrupt_value() {
        run_test(|| {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let (store, fake) = test_store();
                fake.set_raw(PROVIDERS_KEY, r#"{"providers": oops}"#);
                assert!(store.list().await.is_err());
            });
        });
    }
}
