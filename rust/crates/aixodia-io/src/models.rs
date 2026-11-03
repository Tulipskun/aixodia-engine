//! Model discovery: what each configured provider offers.
//!
//! Port of the Go `io/gateway/models.go`.

use std::collections::BTreeMap;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use aixodia_config::Provider as ConfigProvider;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use serde::Deserialize;
use serde_json::json;

use crate::gateway::{write_error, write_json, CachedModels, Gateway};

pub const MODEL_CACHE_TTL: Duration = Duration::from_secs(60);

/// One shared client, like the Go package's `provider.HTTP`.
pub(crate) fn http_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(reqwest::Client::new)
}

#[derive(Debug, Default, Deserialize)]
pub struct ModelsQuery {
    #[serde(default)]
    provider: Option<String>,
}

pub async fn list_models(
    State(g): State<std::sync::Arc<Gateway>>,
    Query(query): Query<ModelsQuery>,
) -> Response {
    let only = query.provider.unwrap_or_default().trim().to_string();
    let providers = aixodia_config::get_providers();

    let filtered: Vec<ConfigProvider> = providers
        .into_iter()
        .filter(|provider| only.is_empty() || provider.name.eq_ignore_ascii_case(&only))
        .collect();
    if !only.is_empty() && filtered.is_empty() {
        return write_error(
            StatusCode::NOT_FOUND,
            "not_found",
            &format!("unknown provider {only:?}"),
        );
    }

    let fetched = futures_util::future::join_all(filtered.iter().cloned().map(|cfg| {
        let gateway = g.clone();
        async move {
            let ids = gateway.provider_models(&cfg).await;
            (cfg, ids)
        }
    }))
    .await;

    let mut models: Vec<(String, String)> = Vec::new();
    let mut failed: BTreeMap<String, String> = BTreeMap::new();
    for (cfg, result) in fetched {
        match result {
            Ok(ids) => {
                for id in ids {
                    models.push((cfg.name.clone(), id));
                }
            }
            Err(err) => {
                failed.insert(cfg.name.clone(), err.to_string());
            }
        }
    }
    models.sort();

    let list: Vec<serde_json::Value> = models
        .into_iter()
        .map(|(provider, id)| json!({"id": id, "provider": provider}))
        .collect();
    let mut payload = json!({"models": list});
    if !failed.is_empty() {
        payload["errors"] = serde_json::to_value(failed).unwrap_or_else(|_| json!({}));
    }
    write_json(StatusCode::OK, payload)
}

impl Gateway {
    pub(crate) async fn provider_models(
        &self,
        cfg: &ConfigProvider,
    ) -> anyhow::Result<Vec<String>> {
        {
            let cache = self.model_list.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(cached) = cache.get(&cfg.name) {
                if cached.fetched.elapsed() < MODEL_CACHE_TTL {
                    return Ok(cached.ids.clone());
                }
            }
        }

        let ids = fetch_models(cfg).await?;

        self.model_list
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                cfg.name.clone(),
                CachedModels {
                    fetched: Instant::now(),
                    ids: ids.clone(),
                },
            );
        Ok(ids)
    }
}

pub async fn fetch_models(cfg: &ConfigProvider) -> anyhow::Result<Vec<String>> {
    // Non-login mode: a provider with no keys is asked unauthenticated, which is
    // how a keyless free tier or a local server is reached. A provider that does
    // need a key answers 401 and that is reported as its own error.
    let endpoint = models_endpoint(cfg)?;

    let mut request = http_client()
        .get(&endpoint)
        .timeout(Duration::from_secs(15))
        .header("Accept", "application/json")
        .header("User-Agent", aixodia_provider::USER_AGENT);
    if let Some(key) = cfg.keys.first() {
        request = request.header("Authorization", format!("Bearer {key}"));
    }

    let response = request.send().await?;
    let status = response.status();
    let body = response.bytes().await?;
    if !status.is_success() {
        return Err(aixodia_provider::APIError::new(
            status.as_u16(),
            aixodia_provider::error_message(&body),
        )
        .into_anyhow());
    }

    parse_models(&body)
}

/// Reads a catalog in either shape: the OpenAI envelope (`{"data": [...]}`) or a
/// bare array. A bare entry may name the model `id` (OpenAI) or `name`
/// (Pollinations), so both are accepted.
fn parse_models(body: &[u8]) -> anyhow::Result<Vec<String>> {
    #[derive(Deserialize)]
    struct Item {
        #[serde(default)]
        id: String,
        #[serde(default)]
        name: String,
    }
    impl Item {
        fn into_id(self) -> String {
            if self.id.is_empty() {
                self.name
            } else {
                self.id
            }
        }
    }
    #[derive(Deserialize)]
    struct Parsed {
        #[serde(default)]
        data: Vec<Item>,
    }

    let unexpected = || anyhow::anyhow!("provider returned an unexpected /models payload");
    let value: serde_json::Value = serde_json::from_slice(body).map_err(|_| unexpected())?;
    let items: Vec<Item> = match value {
        serde_json::Value::Array(_) => serde_json::from_value(value).map_err(|_| unexpected())?,
        serde_json::Value::Object(_) => {
            serde_json::from_value::<Parsed>(value)
                .map_err(|_| unexpected())?
                .data
        }
        _ => return Err(unexpected()),
    };

    Ok(items
        .into_iter()
        .map(Item::into_id)
        .filter(|id| !id.is_empty())
        .collect())
}

/// The catalog URL for a provider: `models_url` when it says the catalog is
/// somewhere other than the chat base.
fn models_endpoint(cfg: &ConfigProvider) -> anyhow::Result<String> {
    let base = if cfg.models_url.trim().is_empty() {
        &cfg.api_url
    } else {
        &cfg.models_url
    };
    aixodia_provider::endpoint(base, "/v1/models", "/models")
}

#[cfg(test)]
mod tests {
    use super::{models_endpoint, parse_models};
    use aixodia_config::Provider as ConfigProvider;

    #[test]
    fn catalog_falls_back_to_the_chat_base() {
        let cfg = ConfigProvider {
            api_url: "https://alpha.test/v1".to_string(),
            ..ConfigProvider::default()
        };
        assert_eq!(
            models_endpoint(&cfg).unwrap(),
            "https://alpha.test/v1/models"
        );
    }

    #[test]
    fn catalog_uses_models_url_when_given() {
        // Zen's shape: chat under …/openai/v1, catalog under …/v1.
        let cfg = ConfigProvider {
            api_url: "https://opencode.ai/inference/openai/v1".to_string(),
            models_url: "https://opencode.ai/inference/v1".to_string(),
            ..ConfigProvider::default()
        };
        assert_eq!(
            models_endpoint(&cfg).unwrap(),
            "https://opencode.ai/inference/v1/models"
        );
    }

    #[test]
    fn catalog_accepts_the_openai_envelope() {
        let ids = parse_models(br#"{"data":[{"id":"m1"},{"id":"m2"}]}"#).unwrap();
        assert_eq!(ids, vec!["m1".to_string(), "m2".to_string()]);
    }

    #[test]
    fn catalog_accepts_a_bare_array_named_like_pollinations() {
        let ids = parse_models(br#"[{"name":"openai-fast"},{"id":"m2"}]"#).unwrap();
        assert_eq!(ids, vec!["openai-fast".to_string(), "m2".to_string()]);
    }

    #[test]
    fn catalog_rejects_an_unexpected_payload() {
        assert!(parse_models(b"\"nope\"").is_err());
    }
}
