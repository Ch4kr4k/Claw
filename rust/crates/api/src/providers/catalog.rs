//! Context and output limits of hosted models, from the models.dev catalog
//! (the one opencode uses): `{"nvidia": {"models": {"deepseek-ai/…":
//! {"limit": {"context": 1000000, "output": 384000}}}}, …}`, keyed by the same
//! provider ids as claw's model prefixes (`nvidia/`, `openrouter/`, `groq/`).
//!
//! The catalog is about 6 MB, so it is kept in `~/.claw/cache/models-dev.json`
//! and fetched again once a day; a failed fetch falls back to the stale copy.
//! `CLAW_MODELS_URL` points at another copy (default `https://models.dev`),
//! `CLAW_MODELS_CATALOG=0` turns the lookup off.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use serde_json::Value;

const DEFAULT_URL: &str = "https://models.dev";
/// How old the cached catalog may get before it is fetched again.
const MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);
/// How long a failed load is remembered before trying again.
const RETRY_AFTER: Duration = Duration::from_secs(300);
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// A model's limits in tokens (`0` when the catalog does not say), and its
/// price in USD per million input and output tokens when listed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CatalogLimit {
    pub context: u32,
    pub output: u32,
    pub cost: Option<(f64, f64)>,
    /// The model expects its reasoning sent back with the turn's earlier
    /// steps (`interleaved` in the catalog, as `reasoning_content`).
    pub replays_reasoning: bool,
    /// Whether the model takes images (`modalities.input` lists "image");
    /// `None` when the catalog does not say.
    pub images: Option<bool>,
}

type Table = HashMap<String, HashMap<String, CatalogLimit>>;

/// A provider the catalog lists with an OpenAI-compatible API (the ones
/// opencode reaches through `@ai-sdk/openai-compatible`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogProvider {
    pub id: String,
    pub name: String,
    /// Base URL of its chat-completions API.
    pub api: String,
    /// Variable holding its key (the catalog's first `env` entry).
    pub key_env: String,
}

struct Catalog {
    limits: Table,
    providers: Vec<CatalogProvider>,
}

/// The limits of `model_id` (as the provider names it, e.g.
/// `deepseek-ai/deepseek-v4.1-flash`) at `provider` (e.g. `nvidia`).
///
/// When this provider's entry is missing, the same model as another
/// provider lists it (`nvidia/nemotron-3-super-120b-a12b` is missing under
/// `nvidia` but listed by OpenRouter), so an unlisted model does not get the
/// provider's small default reply budget (8k: thinking models spent all of
/// it thinking and never acted). A model's own limits do not depend on who
/// serves it; the price does, so it is not borrowed.
#[must_use]
pub fn catalog_limit(provider: &str, model_id: &str) -> Option<CatalogLimit> {
    let catalog = table()?;
    if let Some(limit) = catalog
        .limits
        .get(provider)
        .and_then(|models| models.get(model_id))
    {
        return Some(*limit);
    }
    let base = |id: &str| -> String {
        id.trim_end_matches(":free")
            .rsplit('/')
            .next()
            .unwrap_or(id)
            .to_ascii_lowercase()
    };
    let wanted = base(model_id);
    catalog
        .limits
        .values()
        .flat_map(|models| models.iter())
        .filter(|(id, limit)| base(id) == wanted && limit.context > 0)
        .map(|(_, limit)| CatalogLimit {
            cost: None,
            ..*limit
        })
        .max_by_key(|limit| limit.output)
}

/// Every OpenAI-compatible provider in the catalog. Empty when the catalog
/// is off or cannot be loaded.
#[must_use]
pub fn catalog_providers() -> Vec<CatalogProvider> {
    table()
        .map(|catalog| catalog.providers.clone())
        .unwrap_or_default()
}

fn table() -> Option<Arc<Catalog>> {
    if std::env::var("CLAW_MODELS_CATALOG")
        .is_ok_and(|value| matches!(value.trim(), "0" | "false" | "off" | "no"))
    {
        return None;
    }
    /// When the catalog was last loaded, and what came of it.
    type Loaded = Option<(Instant, Option<Arc<Catalog>>)>;
    static CACHE: OnceLock<Mutex<Loaded>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(None));
    let mut cache = cache.lock().ok()?;
    if let Some((at, table)) = cache.as_ref() {
        if table.is_some() || at.elapsed() < RETRY_AFTER {
            return table.clone();
        }
    }
    let table = load().map(Arc::new);
    *cache = Some((Instant::now(), table.clone()));
    table
}

fn cache_file() -> PathBuf {
    runtime::default_config_home()
        .join("cache")
        .join("models-dev.json")
}

/// The cached catalog while it is fresh, else a new download (saved for next
/// time), else the stale cache.
fn load() -> Option<Catalog> {
    let file = cache_file();
    let cached = std::fs::read_to_string(&file).ok();
    let fresh = std::fs::metadata(&file)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_some_and(|age| age < MAX_AGE);
    if fresh {
        if let Some(table) = cached.as_deref().and_then(parse_catalog) {
            return Some(table);
        }
    }
    if let Some(body) = fetch() {
        if let Some(table) = parse_catalog(&body) {
            if let Some(dir) = file.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(&file, body);
            return Some(table);
        }
    }
    cached.as_deref().and_then(parse_catalog)
}

/// Downloads `<url>/api.json` on its own thread and runtime, so callers can
/// be synchronous code inside or outside a Tokio runtime.
fn fetch() -> Option<String> {
    let base = std::env::var("CLAW_MODELS_URL")
        .ok()
        .map(|value| value.trim().trim_end_matches('/').to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_URL.to_string());
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .ok()?;
        runtime.block_on(async {
            let http = reqwest::Client::builder()
                .timeout(FETCH_TIMEOUT)
                .build()
                .ok()?;
            let response = http.get(format!("{base}/api.json")).send().await.ok()?;
            if !response.status().is_success() {
                return None;
            }
            response.text().await.ok()
        })
    })
    .join()
    .ok()
    .flatten()
}

fn parse_catalog(body: &str) -> Option<Catalog> {
    let root: Value = serde_json::from_str(body).ok()?;
    let providers = root.as_object()?;
    let mut compatible = providers
        .iter()
        .filter(|(_, entry)| {
            entry.get("npm").and_then(Value::as_str) == Some("@ai-sdk/openai-compatible")
        })
        .filter_map(|(id, entry)| {
            let api = entry.get("api")?.as_str()?.trim().trim_end_matches('/');
            let key_env = entry.get("env")?.as_array()?.first()?.as_str()?;
            (api.starts_with("https://") || api.starts_with("http://")).then(|| CatalogProvider {
                id: id.to_ascii_lowercase(),
                name: entry
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or(id)
                    .to_string(),
                api: api.to_string(),
                key_env: key_env.to_string(),
            })
        })
        .collect::<Vec<_>>();
    compatible.sort_by(|a, b| a.id.cmp(&b.id));
    let table = providers
        .iter()
        .filter_map(|(provider, entry)| {
            let models = entry.get("models")?.as_object()?;
            let limits = models
                .iter()
                .filter_map(|(id, model)| {
                    let limit = model.get("limit")?;
                    let read = |key: &str| {
                        limit
                            .get(key)
                            .and_then(Value::as_u64)
                            .and_then(|n| u32::try_from(n).ok())
                            .unwrap_or(0)
                    };
                    let cost = model.get("cost").and_then(|cost| {
                        Some((cost.get("input")?.as_f64()?, cost.get("output")?.as_f64()?))
                    });
                    let replays_reasoning = match model.get("interleaved") {
                        Some(Value::Bool(on)) => *on,
                        Some(Value::Object(spec)) => {
                            spec.get("field").and_then(Value::as_str) == Some("reasoning_content")
                        }
                        _ => false,
                    };
                    let images = model
                        .pointer("/modalities/input")
                        .and_then(Value::as_array)
                        .map(|input| input.iter().any(|kind| kind.as_str() == Some("image")));
                    let found = CatalogLimit {
                        context: read("context"),
                        output: read("output"),
                        cost,
                        replays_reasoning,
                        images,
                    };
                    (found.context > 0 || found.output > 0).then(|| (id.clone(), found))
                })
                .collect::<HashMap<_, _>>();
            Some((provider.clone(), limits))
        })
        .collect::<Table>();
    (!table.is_empty()).then_some(Catalog {
        limits: table,
        providers: compatible,
    })
}

#[cfg(test)]
mod tests {
    use super::{parse_catalog, CatalogLimit};

    #[test]
    fn reads_limits_per_provider_and_model() {
        let body = r#"{
            "nvidia": {"id": "nvidia", "name": "Nvidia", "npm": "@ai-sdk/openai-compatible",
                "api": "https://integrate.api.nvidia.com/v1", "env": ["NVIDIA_API_KEY"], "models": {
                "deepseek-ai/deepseek-v4.1-flash": {"limit": {"context": 1000000, "output": 384000},
                    "cost": {"input": 0.3, "output": 1.2},
                    "modalities": {"input": ["text"], "output": ["text"]}},
                "vl": {"limit": {"context": 1000, "output": 100},
                    "modalities": {"input": ["text", "image"], "output": ["text"]}},
                "no-limit": {"name": "x"}
            }},
            "openrouter": {"models": {
                "poolside/laguna-s-2.1:free": {"limit": {"context": 262144, "output": 32768}}
            }}
        }"#;
        let catalog = parse_catalog(body).expect("catalog");
        // Only OpenAI-compatible providers with an API URL are offered.
        assert_eq!(
            catalog.providers,
            [super::CatalogProvider {
                id: "nvidia".to_string(),
                name: "Nvidia".to_string(),
                api: "https://integrate.api.nvidia.com/v1".to_string(),
                key_env: "NVIDIA_API_KEY".to_string(),
            }]
        );
        let table = catalog.limits;
        assert_eq!(
            table["nvidia"].get("deepseek-ai/deepseek-v4.1-flash"),
            Some(&CatalogLimit {
                context: 1_000_000,
                output: 384_000,
                cost: Some((0.3, 1.2)),
                replays_reasoning: false,
                images: Some(false),
            })
        );
        assert_eq!(table["nvidia"].get("no-limit"), None);
        assert_eq!(table["nvidia"]["vl"].images, Some(true));
        assert_eq!(
            table["openrouter"]["poolside/laguna-s-2.1:free"].images,
            None
        );
        assert_eq!(
            table["openrouter"]["poolside/laguna-s-2.1:free"].context,
            262_144
        );
        assert!(parse_catalog("{}").is_none());
        assert!(parse_catalog("not json").is_none());
    }

    #[test]
    fn borrows_a_models_limits_from_another_provider() {
        let body = r#"{
            "nvidia": {"models": {"other": {"limit": {"context": 1000, "output": 100}}}},
            "openrouter": {"models": {
                "nvidia/nemotron-3-super-120b-a12b:free": {"limit": {"context": 262144, "output": 235929},
                    "cost": {"input": 0, "output": 0}}
            }}
        }"#;
        let catalog = parse_catalog(body).expect("catalog");
        let base = |id: &str| {
            id.trim_end_matches(":free")
                .rsplit('/')
                .next()
                .unwrap_or(id)
                .to_string()
        };
        let found = catalog
            .limits
            .values()
            .flat_map(|models| models.iter())
            .find(|(id, _)| base(id) == base("nvidia/nemotron-3-super-120b-a12b"))
            .map(|(_, limit)| limit.output);
        assert_eq!(found, Some(235_929));
    }
}
