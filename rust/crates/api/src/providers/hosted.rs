//! Hosted APIs that speak the OpenAI chat-completions shape and are reached
//! by a model prefix: `zai/glm-4.6`, `openrouter/anthropic/claude-sonnet-4`,
//! `deepseek/deepseek-v4-pro`, … Each has its own key and base-URL variables,
//! so several can be configured side by side with OpenAI and Anthropic.
//!
//! Besides the built-in ones, `customProviders` in settings.json adds any
//! other OpenAI-compatible endpoint under its own prefix, so it never
//! competes with `local/` (the local server) or another provider:
//!
//! ```json
//! "customProviders": {
//!   "together": { "baseUrl": "https://api.together.xyz/v1", "apiKeyEnv": "TOGETHER_API_KEY" }
//! }
//! ```
//!
//! `together/meta-llama/Llama-3.3-70B-Instruct-Turbo` then goes to Together
//! as `meta-llama/Llama-3.3-70B-Instruct-Turbo`. Optional: `label`,
//! `apiKey` (the key itself, when no variable holds it),
//! `maxOutputTokens` (default 8192).

use std::collections::HashMap;
use std::sync::OnceLock;

use serde_json::Value;

use super::openai_compat::OpenAiCompatConfig;

#[derive(Debug, Clone, Copy)]
pub struct HostedOpenAiProvider {
    /// The model prefix (`zai/…`) and the `provider.kind` saved by `/login`.
    pub id: &'static str,
    pub label: &'static str,
    /// Bare model names routed here too (`glm-4.6`), but only while this
    /// provider's key is set, so they keep working through `OPENAI_BASE_URL`
    /// otherwise.
    pub bare_model_prefixes: &'static [&'static str],
    pub config: OpenAiCompatConfig,
    pub credential_env_vars: &'static [&'static str],
    /// Reply budget for models without a known limit: asking for more than
    /// a model allows is an error on most of these APIs, while a reply that
    /// reaches the budget is continued.
    pub default_max_output_tokens: u32,
    /// Other endpoints worth knowing about, shown by `/login`.
    pub base_url_note: Option<&'static str>,
}

const OPENAI_MAX_REQUEST_BODY_BYTES: usize = 104_857_600;

pub const HOSTED_OPENAI_PROVIDERS: &[HostedOpenAiProvider] = &[
    HostedOpenAiProvider {
        id: "zai",
        label: "Z.ai (GLM)",
        bare_model_prefixes: &["glm-"],
        config: OpenAiCompatConfig {
            provider_name: "Z.ai",
            api_key_env: "ZAI_API_KEY",
            base_url_env: "ZAI_BASE_URL",
            default_base_url: "https://api.z.ai/api/paas/v4",
            max_request_body_bytes: OPENAI_MAX_REQUEST_BODY_BYTES,
        },
        credential_env_vars: &["ZAI_API_KEY"],
        default_max_output_tokens: 32_768,
        base_url_note: Some(
            "GLM Coding Plan: https://api.z.ai/api/coding/paas/v4 · China: https://open.bigmodel.cn/api/paas/v4",
        ),
    },
    HostedOpenAiProvider {
        id: "openrouter",
        label: "OpenRouter",
        bare_model_prefixes: &[],
        config: OpenAiCompatConfig {
            provider_name: "OpenRouter",
            api_key_env: "OPENROUTER_API_KEY",
            base_url_env: "OPENROUTER_BASE_URL",
            default_base_url: "https://openrouter.ai/api/v1",
            max_request_body_bytes: OPENAI_MAX_REQUEST_BODY_BYTES,
        },
        credential_env_vars: &["OPENROUTER_API_KEY"],
        default_max_output_tokens: 32_768,
        base_url_note: None,
    },
    HostedOpenAiProvider {
        id: "deepseek",
        label: "DeepSeek",
        bare_model_prefixes: &["deepseek-"],
        config: OpenAiCompatConfig {
            provider_name: "DeepSeek",
            api_key_env: "DEEPSEEK_API_KEY",
            base_url_env: "DEEPSEEK_BASE_URL",
            default_base_url: "https://api.deepseek.com/v1",
            max_request_body_bytes: OPENAI_MAX_REQUEST_BODY_BYTES,
        },
        credential_env_vars: &["DEEPSEEK_API_KEY"],
        default_max_output_tokens: 32_768,
        base_url_note: None,
    },
    HostedOpenAiProvider {
        id: "gemini",
        label: "Google Gemini",
        bare_model_prefixes: &["gemini-"],
        config: OpenAiCompatConfig {
            provider_name: "Gemini",
            api_key_env: "GEMINI_API_KEY",
            base_url_env: "GEMINI_BASE_URL",
            default_base_url: "https://generativelanguage.googleapis.com/v1beta/openai",
            max_request_body_bytes: 20 * 1024 * 1024,
        },
        credential_env_vars: &["GEMINI_API_KEY"],
        default_max_output_tokens: 32_768,
        base_url_note: None,
    },
    HostedOpenAiProvider {
        id: "groq",
        label: "Groq",
        bare_model_prefixes: &[],
        config: OpenAiCompatConfig {
            provider_name: "Groq",
            api_key_env: "GROQ_API_KEY",
            base_url_env: "GROQ_BASE_URL",
            default_base_url: "https://api.groq.com/openai/v1",
            max_request_body_bytes: OPENAI_MAX_REQUEST_BODY_BYTES,
        },
        credential_env_vars: &["GROQ_API_KEY"],
        default_max_output_tokens: 8_192,
        base_url_note: None,
    },
    HostedOpenAiProvider {
        id: "mistral",
        label: "Mistral",
        bare_model_prefixes: &["mistral-", "codestral-", "devstral-", "magistral-"],
        config: OpenAiCompatConfig {
            provider_name: "Mistral",
            api_key_env: "MISTRAL_API_KEY",
            base_url_env: "MISTRAL_BASE_URL",
            default_base_url: "https://api.mistral.ai/v1",
            max_request_body_bytes: OPENAI_MAX_REQUEST_BODY_BYTES,
        },
        credential_env_vars: &["MISTRAL_API_KEY"],
        default_max_output_tokens: 8_192,
        base_url_note: None,
    },
    HostedOpenAiProvider {
        id: "nvidia",
        label: "NVIDIA NIM",
        bare_model_prefixes: &[],
        config: OpenAiCompatConfig {
            provider_name: "NVIDIA",
            api_key_env: "NVIDIA_API_KEY",
            base_url_env: "NVIDIA_BASE_URL",
            default_base_url: "https://integrate.api.nvidia.com/v1",
            max_request_body_bytes: OPENAI_MAX_REQUEST_BODY_BYTES,
        },
        credential_env_vars: &["NVIDIA_API_KEY"],
        default_max_output_tokens: 8_192,
        base_url_note: Some("Self-hosted NIM: http://<host>:8000/v1"),
    },
];

/// Built-in providers, then the ones from `customProviders` in settings.
pub fn all_hosted_providers() -> impl Iterator<Item = &'static HostedOpenAiProvider> {
    HOSTED_OPENAI_PROVIDERS
        .iter()
        .chain(custom().providers.iter())
}

struct Custom {
    providers: Vec<HostedOpenAiProvider>,
    /// Keys written in settings (`apiKey`), by the variable they stand in for.
    keys: HashMap<String, String>,
}

/// The `customProviders` from `~/.claw/settings.json` and the project's
/// `.claw/settings.json` (the project wins), read once per process.
fn custom() -> &'static Custom {
    static CUSTOM: OnceLock<Custom> = OnceLock::new();
    CUSTOM.get_or_init(|| {
        let mut entries = serde_json::Map::new();
        let files = [
            runtime::default_config_home().join("settings.json"),
            std::path::PathBuf::from(".claw").join("settings.json"),
        ];
        for file in files {
            let Some(found) = std::fs::read_to_string(file)
                .ok()
                .and_then(|text| serde_json::from_str::<Value>(&text).ok())
                .and_then(|root| {
                    root.get("customProviders")
                        .and_then(Value::as_object)
                        .cloned()
                })
            else {
                continue;
            };
            entries.extend(found);
        }
        parse_custom(&entries)
    })
}

fn leak(text: String) -> &'static str {
    Box::leak(text.into_boxed_str())
}

fn parse_custom(entries: &serde_json::Map<String, Value>) -> Custom {
    let mut providers = Vec::new();
    let mut keys = HashMap::new();
    for (id, entry) in entries {
        let id = id.trim().to_ascii_lowercase();
        let Some(base_url) = entry.get("baseUrl").and_then(Value::as_str) else {
            continue;
        };
        // `local` is the local server's prefix; built-in ids stay built in.
        if id.is_empty() || id.contains('/') || id == "local" || hosted_builtin(&id).is_some() {
            continue;
        }
        let stem: String = id
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() {
                    c.to_ascii_uppercase()
                } else {
                    '_'
                }
            })
            .collect();
        let key_env = entry
            .get("apiKeyEnv")
            .and_then(Value::as_str)
            .map_or_else(|| format!("{stem}_API_KEY"), str::to_string);
        if let Some(key) = entry.get("apiKey").and_then(Value::as_str) {
            keys.insert(key_env.clone(), key.to_string());
        }
        let label = entry
            .get("label")
            .and_then(Value::as_str)
            .map_or_else(|| id.clone(), str::to_string);
        let key_env = leak(key_env);
        providers.push(HostedOpenAiProvider {
            id: leak(id.clone()),
            label: leak(label.clone()),
            bare_model_prefixes: &[],
            config: OpenAiCompatConfig {
                provider_name: leak(label),
                api_key_env: key_env,
                base_url_env: leak(format!("{stem}_BASE_URL")),
                default_base_url: leak(base_url.trim_end_matches('/').to_string()),
                max_request_body_bytes: OPENAI_MAX_REQUEST_BODY_BYTES,
            },
            credential_env_vars: Box::leak(vec![key_env].into_boxed_slice()),
            default_max_output_tokens: entry
                .get("maxOutputTokens")
                .and_then(Value::as_u64)
                .and_then(|tokens| u32::try_from(tokens).ok())
                .unwrap_or(8_192),
            base_url_note: None,
        });
    }
    Custom { providers, keys }
}

/// A custom provider's key written in settings, for the variable `env`.
#[must_use]
pub fn custom_api_key(env: &str) -> Option<String> {
    custom().keys.get(env).cloned()
}

fn hosted_builtin(id: &str) -> Option<&'static HostedOpenAiProvider> {
    HOSTED_OPENAI_PROVIDERS
        .iter()
        .find(|provider| provider.id.eq_ignore_ascii_case(id))
}

/// The provider saved as `provider.kind` = `id`.
#[must_use]
pub fn hosted_openai_provider(id: &str) -> Option<&'static HostedOpenAiProvider> {
    all_hosted_providers().find(|provider| provider.id.eq_ignore_ascii_case(id))
}

/// The provider a model name routes to: by its `id/` prefix, or by a bare
/// name prefix while that provider's key is set.
#[must_use]
pub fn hosted_openai_provider_for_model(model: &str) -> Option<&'static HostedOpenAiProvider> {
    let lowered = model.trim().to_ascii_lowercase();
    if let Some(provider) = all_hosted_providers().find(|provider| {
        lowered
            .strip_prefix(provider.id)
            .and_then(|rest| rest.strip_prefix('/'))
            .is_some_and(|rest| !rest.is_empty())
    }) {
        return Some(provider);
    }
    HOSTED_OPENAI_PROVIDERS.iter().find(|provider| {
        provider
            .bare_model_prefixes
            .iter()
            .any(|prefix| lowered.starts_with(prefix))
            && std::env::var(provider.config.api_key_env)
                .is_ok_and(|value| !value.trim().is_empty())
    })
}

/// The model name the provider expects: `zai/glm-4.6` is sent as `glm-4.6`,
/// `openrouter/anthropic/claude-sonnet-4` as `anthropic/claude-sonnet-4`.
#[must_use]
pub fn strip_hosted_prefix(model: &str) -> Option<&str> {
    let (prefix, rest) = model.split_once('/')?;
    hosted_openai_provider(prefix).map(|_| rest)
}

/// Lists the model ids an API offers (`GET <base>/models`), for `/login`.
/// `anthropic` uses Anthropic's headers and `/v1/models`; everything else
/// the OpenAI shape. Both answer `{"data": [{"id": …}, …]}`.
pub async fn list_models(
    base_url: &str,
    api_key: &str,
    anthropic: bool,
) -> Result<Vec<String>, String> {
    let base = base_url.trim().trim_end_matches('/');
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|error| error.to_string())?;
    let request = if anthropic {
        let base = base.strip_suffix("/v1").unwrap_or(base);
        http.get(format!("{base}/v1/models?limit=100"))
            .header("x-api-key", api_key)
            .header("anthropic-version", "2023-06-01")
    } else {
        let request = http.get(format!("{base}/models"));
        if api_key.trim().is_empty() {
            request
        } else {
            request.bearer_auth(api_key)
        }
    };
    let response = request.send().await.map_err(|error| error.to_string())?;
    let status = response.status();
    let body = response.text().await.map_err(|error| error.to_string())?;
    if !status.is_success() {
        let detail: String = body.chars().take(200).collect();
        return Err(format!("HTTP {status}: {detail}"));
    }
    parse_model_ids(&body)
}

fn parse_model_ids(body: &str) -> Result<Vec<String>, String> {
    let json: serde_json::Value =
        serde_json::from_str(body).map_err(|error| format!("unexpected reply: {error}"))?;
    let entries = json
        .get("data")
        .or_else(|| json.get("models"))
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "unexpected reply: no model list".to_string())?;
    let mut ids = entries
        .iter()
        .filter_map(|entry| {
            entry
                .get("id")
                .or_else(|| entry.get("name"))
                .and_then(serde_json::Value::as_str)
        })
        .map(|id| id.trim_start_matches("models/").to_string())
        .collect::<Vec<_>>();
    ids.sort();
    ids.dedup();
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::{
        hosted_openai_provider, hosted_openai_provider_for_model, parse_model_ids,
        strip_hosted_prefix,
    };

    #[test]
    fn parses_model_lists() {
        let openai = r#"{"object":"list","data":[{"id":"glm-4.6"},{"id":"glm-4.5-air"}]}"#;
        assert_eq!(
            parse_model_ids(openai),
            Ok(vec!["glm-4.5-air".to_string(), "glm-4.6".to_string()])
        );
        let gemini = r#"{"data":[{"id":"models/gemini-2.5-pro"}]}"#;
        assert_eq!(
            parse_model_ids(gemini),
            Ok(vec!["gemini-2.5-pro".to_string()])
        );
        assert!(parse_model_ids("{}").is_err());
    }

    #[test]
    fn prefixed_models_route_to_their_provider() {
        let id = |model: &str| hosted_openai_provider_for_model(model).map(|p| p.id);
        assert_eq!(id("zai/glm-4.6"), Some("zai"));
        assert_eq!(
            id("openrouter/anthropic/claude-sonnet-4"),
            Some("openrouter")
        );
        assert_eq!(id("deepseek/deepseek-v4-pro"), Some("deepseek"));
        assert_eq!(id("gemini/gemini-2.5-pro"), Some("gemini"));
        assert_eq!(id("groq/llama-3.3-70b-versatile"), Some("groq"));
        assert_eq!(id("mistral/mistral-large-latest"), Some("mistral"));
        assert_eq!(id("zai/"), None);
        assert_eq!(id("local/glm-4.6"), None);
        assert_eq!(id("openai/gpt-4.1"), None);
    }

    #[test]
    fn strips_only_the_provider_prefix() {
        assert_eq!(strip_hosted_prefix("zai/glm-4.6"), Some("glm-4.6"));
        assert_eq!(
            strip_hosted_prefix("openrouter/anthropic/claude-sonnet-4"),
            Some("anthropic/claude-sonnet-4")
        );
        assert_eq!(strip_hosted_prefix("openai/gpt-4.1"), None);
        assert_eq!(strip_hosted_prefix("glm-4.6"), None);
    }

    #[test]
    fn looks_up_saved_provider_kinds() {
        assert_eq!(
            hosted_openai_provider("zai").map(|p| p.config.api_key_env),
            Some("ZAI_API_KEY")
        );
        assert!(hosted_openai_provider("anthropic").is_none());
    }

    #[test]
    fn nvidia_has_its_own_prefix() {
        let provider =
            super::hosted_openai_provider_for_model("nvidia/qwen/qwen3-coder-480b-a35b-instruct")
                .expect("nvidia/ routes to NVIDIA");
        assert_eq!(provider.config.api_key_env, "NVIDIA_API_KEY");
        assert_eq!(
            super::strip_hosted_prefix("nvidia/qwen/qwen3-coder-480b-a35b-instruct"),
            Some("qwen/qwen3-coder-480b-a35b-instruct")
        );
        assert!(super::hosted_openai_provider_for_model("local/qwen36-35b-a3b").is_none());
    }

    #[test]
    fn custom_providers_come_from_settings() {
        let entries = serde_json::json!({
            "Together": { "baseUrl": "https://api.together.xyz/v1/", "apiKeyEnv": "TOGETHER_API_KEY" },
            "my-vllm": { "baseUrl": "http://gpu-box:8000/v1", "apiKey": "secret", "maxOutputTokens": 4096 },
            "local": { "baseUrl": "http://x/v1" },
            "zai": { "baseUrl": "http://x/v1" },
            "nourl": {}
        });
        let custom = super::parse_custom(entries.as_object().unwrap());
        let ids: Vec<&str> = custom.providers.iter().map(|p| p.id).collect();
        assert_eq!(ids, ["together", "my-vllm"]);
        let together = &custom.providers[0];
        assert_eq!(
            together.config.default_base_url,
            "https://api.together.xyz/v1"
        );
        assert_eq!(together.config.api_key_env, "TOGETHER_API_KEY");
        assert_eq!(together.config.base_url_env, "TOGETHER_BASE_URL");
        let vllm = &custom.providers[1];
        assert_eq!(vllm.config.api_key_env, "MY_VLLM_API_KEY");
        assert_eq!(vllm.default_max_output_tokens, 4096);
        assert_eq!(
            custom.keys.get("MY_VLLM_API_KEY").map(String::as_str),
            Some("secret")
        );
    }
}
