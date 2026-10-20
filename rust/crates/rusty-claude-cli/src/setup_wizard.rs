use std::io::{self, IsTerminal, Write};

use runtime::{save_user_provider_settings, ConfigLoader, RuntimeProviderConfig};

use crate::ask::{self, Pick};
use crate::DEFAULT_LOCAL_BASE_URL;

/// Esc (or Ctrl+C) at any step: nothing is saved.
#[derive(Debug)]
struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("login cancelled")
    }
}

impl std::error::Error for Cancelled {}

/// A choice from a list (arrow keys, Enter, Esc cancels the login).
fn choose(
    question: &str,
    options: &[(String, Option<String>)],
    default: usize,
    allow_other: bool,
) -> Result<Pick, Box<dyn std::error::Error>> {
    match ask::pick(question, options, default, allow_other)? {
        Pick::Cancelled => Err(Box::new(Cancelled)),
        pick => Ok(pick),
    }
}

/// A typed answer (Enter accepts, an empty answer keeps the current value,
/// Esc cancels the login).
fn text(prompt: &str, hidden: bool) -> Result<String, Box<dyn std::error::Error>> {
    ask::ask_text(prompt, hidden)?
        .map(|answer| answer.trim().to_string())
        .ok_or_else(|| Box::new(Cancelled) as Box<dyn std::error::Error>)
}

/// One provider `/login` can set up.
struct ProviderChoice {
    label: &'static str,
    /// Saved as `provider.kind`; `apply_saved_provider_credentials` turns
    /// it back into the environment variables below.
    kind: &'static str,
    key_env: &'static str,
    url_env: &'static str,
    default_url: &'static str,
    /// Put before a listed model id so it routes to this provider.
    model_prefix: &'static str,
    /// Shown when the model list cannot be fetched.
    suggested_models: &'static [&'static str],
    key_optional: bool,
    note: Option<&'static str>,
}

fn provider_choices() -> Vec<ProviderChoice> {
    let mut choices = vec![
        ProviderChoice {
            label: "Anthropic (Claude API)",
            kind: "anthropic",
            key_env: "ANTHROPIC_API_KEY",
            url_env: "ANTHROPIC_BASE_URL",
            default_url: "https://api.anthropic.com",
            model_prefix: "",
            suggested_models: &["opus", "sonnet", "haiku"],
            key_optional: false,
            note: None,
        },
        ProviderChoice {
            label: "OpenAI",
            kind: "openai",
            key_env: "OPENAI_API_KEY",
            url_env: "OPENAI_BASE_URL",
            default_url: "https://api.openai.com/v1",
            model_prefix: "openai/",
            suggested_models: &["openai/gpt-5.4", "openai/gpt-5.4-mini", "openai/gpt-4.1"],
            key_optional: false,
            note: None,
        },
    ];
    choices.extend(api::all_hosted_providers().map(|hosted| ProviderChoice {
        label: hosted.label,
        kind: hosted.id,
        key_env: hosted.config.api_key_env,
        url_env: hosted.config.base_url_env,
        default_url: hosted.config.default_base_url,
        model_prefix: hosted_model_prefix(hosted.id),
        suggested_models: &[],
        key_optional: false,
        note: hosted.base_url_note,
    }));
    choices.extend([
        ProviderChoice {
            label: "xAI (Grok)",
            kind: "xai",
            key_env: "XAI_API_KEY",
            url_env: "XAI_BASE_URL",
            default_url: "https://api.x.ai/v1",
            model_prefix: "xai/",
            suggested_models: &["xai/grok-3", "xai/grok-3-mini"],
            key_optional: false,
            note: None,
        },
        ProviderChoice {
            label: "Alibaba DashScope (Qwen, Kimi)",
            kind: "dashscope",
            key_env: "DASHSCOPE_API_KEY",
            url_env: "DASHSCOPE_BASE_URL",
            default_url: "https://dashscope.aliyuncs.com/compatible-mode/v1",
            model_prefix: "",
            suggested_models: &["qwen/qwen-plus", "qwen/qwen-max", "kimi/kimi-k2.5"],
            key_optional: false,
            note: Some("Model ids are used as qwen/<id> or kimi/<id>."),
        },
        ProviderChoice {
            label: "Local or custom OpenAI-compatible server (llama.cpp, Ollama, vLLM, LM Studio)",
            kind: "local",
            key_env: "CLAW_LOCAL_API_KEY",
            url_env: "CLAW_LOCAL_BASE_URL",
            default_url: DEFAULT_LOCAL_BASE_URL,
            model_prefix: "local/",
            suggested_models: &[],
            key_optional: true,
            note: Some(
                "Ollama: http://127.0.0.1:11434/v1 · LM Studio: http://127.0.0.1:1234/v1 · a remote server needs a key",
            ),
        },
    ]);
    choices
}

/// The model prefix that routes to a hosted provider: its id and a slash
/// (`zai/`, `nvidia/`, or a `customProviders` name). Built once per id.
fn hosted_model_prefix(id: &str) -> &'static str {
    static PREFIXES: std::sync::Mutex<Vec<&'static str>> = std::sync::Mutex::new(Vec::new());
    let wanted = format!("{id}/");
    let mut prefixes = PREFIXES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(prefix) = prefixes.iter().find(|prefix| **prefix == wanted) {
        return prefix;
    }
    let prefix: &'static str = Box::leak(wanted.into_boxed_str());
    prefixes.push(prefix);
    prefix
}

pub fn run_setup_wizard() -> Result<(), Box<dyn std::error::Error>> {
    if !io::stdin().is_terminal() {
        return Err("setup wizard requires an interactive terminal".into());
    }
    match run_steps() {
        Err(error) if error.is::<Cancelled>() => {
            println!("  Login cancelled; nothing was changed.");
            println!();
            Ok(())
        }
        result => result,
    }
}

fn run_steps() -> Result<(), Box<dyn std::error::Error>> {
    let (current, active_model) = load_current();
    let choices = provider_choices();
    // The model in use tells which provider is current (`local/qwen…` is the
    // local server), even when an older /login saved another provider; the
    // saved one counts when the model name does not say.
    let current_kind = active_model
        .as_deref()
        .and_then(|model| kind_for_model(model, &choices))
        .or(current.kind())
        .unwrap_or("anthropic")
        .to_string();

    println!();
    println!("  \x1b[1mClaw Code Setup Wizard\x1b[0m");
    println!("  Configure your provider, API key, and model.");
    println!(
        "  ↑↓ and Enter choose, Enter on an empty answer keeps the current value, Esc cancels.\n"
    );

    let choice = prompt_provider(&choices, &current_kind)?;
    // The saved key and URL belong to the saved provider: offering them for
    // another one would send one provider's key to another.
    let same_provider = current.kind() == Some(choice.kind);
    let api_key = prompt_api_key(choice, &current, same_provider)?;
    let base_url = prompt_base_url(choice, &current, same_provider)?;
    let models = fetch_models(choice, &api_key, base_url.as_deref());
    // The model in use counts as current when it belongs to this provider.
    let active_here = active_model
        .as_deref()
        .filter(|model| kind_for_model(model, &choices) == Some(choice.kind));
    let model = prompt_model(choice, &current, same_provider, active_here, &models)?;
    let fast_model = prompt_fast_model(model.as_deref(), &models)?;

    save_user_provider_settings(choice.kind, &api_key, base_url.as_deref(), model.as_deref())?;

    match fast_model {
        FastModel::Keep => {}
        FastModel::SameAsMain => remove_settings_field("subagentModel")?,
        FastModel::Model(fast) => save_settings_field("subagentModel", &fast)?,
    }

    println!();
    println!("  \x1b[32mProvider saved to ~/.claw/settings.json\x1b[0m");
    println!(
        "  Run \x1b[1m/model {}\x1b[0m or restart claw to activate.",
        model.as_deref().unwrap_or(choice.kind)
    );
    println!();

    Ok(())
}

/// The saved provider settings and the model in use (aliases resolved).
fn load_current() -> (RuntimeProviderConfig, Option<String>) {
    let cwd = std::env::current_dir().unwrap_or_default();
    let Ok(config) = ConfigLoader::default_for(&cwd).load() else {
        return (RuntimeProviderConfig::default(), None);
    };
    let model = config
        .model()
        .filter(|model| !model.trim().is_empty())
        .map(crate::resolve_model_alias_with_config);
    (config.provider().clone(), model)
}

/// The provider a model name routes to, among the `/login` choices.
fn kind_for_model(model: &str, choices: &[ProviderChoice]) -> Option<&'static str> {
    let model = model.trim().to_ascii_lowercase();
    if model.starts_with("qwen/") || model.starts_with("kimi/") {
        return Some("dashscope");
    }
    if let Some(choice) = choices
        .iter()
        .find(|choice| !choice.model_prefix.is_empty() && model.starts_with(choice.model_prefix))
    {
        return Some(choice.kind);
    }
    if model.starts_with("gpt-") {
        return Some("openai");
    }
    (model.starts_with("claude") || model.starts_with("anthropic/")).then_some("anthropic")
}

fn prompt_provider<'a>(
    choices: &'a [ProviderChoice],
    current_kind: &str,
) -> Result<&'a ProviderChoice, Box<dyn std::error::Error>> {
    let options: Vec<(String, Option<String>)> = choices
        .iter()
        .map(|choice| {
            let mut note = choice.key_env.to_string();
            if choice.kind == current_kind {
                note.insert_str(0, "current · ");
            }
            (choice.label.to_string(), Some(note))
        })
        .collect();
    let default = choices
        .iter()
        .position(|choice| choice.kind == current_kind)
        .unwrap_or(0);
    // With the models.dev providers the list is long: type to filter it.
    match ask::pick_filtered("Provider (type to filter)", &options, default)? {
        Pick::Index(index) => Ok(&choices[index]),
        _ => Err(Box::new(Cancelled)),
    }
}

fn prompt_api_key(
    choice: &ProviderChoice,
    current: &RuntimeProviderConfig,
    same_provider: bool,
) -> Result<String, Box<dyn std::error::Error>> {
    let env_var = choice.key_env;
    let current_key = if same_provider {
        current.api_key().filter(|key| !key.is_empty())
    } else {
        None
    };
    let hint = match current_key {
        Some(key) => format!("[{}]", mask_key(key)),
        None if choice.key_optional => "(optional)".to_string(),
        None => "(none)".to_string(),
    };

    let env_set = std::env::var(env_var).ok().is_some_and(|v| !v.is_empty());
    if env_set {
        println!("  {env_var} is set in environment (will take priority over stored key)");
    }

    let input = text(&format!("API key ({env_var}) {hint}:"), true)?;
    let key = if input.is_empty() {
        current_key.unwrap_or("").to_string()
    } else {
        input
    };

    if key.is_empty() && !env_set && !choice.key_optional {
        eprintln!(
            "  \x1b[33mWarning: no API key configured. Set {env_var} or re-run /login.\x1b[0m"
        );
    }

    Ok(key)
}

fn mask_key(key: &str) -> String {
    let chars = key.chars().collect::<Vec<_>>();
    if chars.len() > 4 {
        format!(
            "****{}",
            chars[chars.len() - 4..].iter().collect::<String>()
        )
    } else {
        "****".to_string()
    }
}

fn prompt_base_url(
    choice: &ProviderChoice,
    current: &RuntimeProviderConfig,
    same_provider: bool,
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    let default_url = choice.default_url;
    let current_url = same_provider
        .then(|| current.base_url())
        .flatten()
        .filter(|url| !url.is_empty())
        .unwrap_or(default_url);

    let env_var = choice.url_env;
    if std::env::var(env_var).ok().is_some_and(|v| !v.is_empty()) {
        println!("  {env_var} is set in environment (will take priority over stored URL)");
    }
    if let Some(note) = choice.note {
        println!("    {note}");
    }

    let input = text(&format!("Base URL [{current_url}]:"), false)?;
    let url = if input.is_empty() {
        current_url.to_string()
    } else {
        input
    };
    // The local server's URL is always saved; for the others the default
    // is implied.
    if url.trim_end_matches('/') == default_url && choice.kind != "local" {
        Ok(None)
    } else {
        Ok(Some(url))
    }
}

/// The provider's model ids, from its `/models` endpoint, with the prefix
/// that routes them here. Empty when the list cannot be fetched.
fn fetch_models(choice: &ProviderChoice, api_key: &str, base_url: Option<&str>) -> Vec<String> {
    let key = if api_key.is_empty() {
        std::env::var(choice.key_env).unwrap_or_default()
    } else {
        api_key.to_string()
    };
    if key.is_empty() && !choice.key_optional {
        return Vec::new();
    }
    let url = base_url.unwrap_or(choice.default_url).to_string();
    print!("  Fetching models from {url} …");
    let _ = io::stdout().flush();
    let result = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())
        .and_then(|runtime| {
            runtime.block_on(api::list_models(&url, &key, choice.kind == "anthropic"))
        });
    match result {
        Ok(ids) => {
            println!(" {} found", ids.len());
            ids.into_iter()
                .map(|id| model_name_for(choice, &id))
                .collect()
        }
        Err(error) => {
            println!(" \x1b[33mfailed\x1b[0m ({error})");
            Vec::new()
        }
    }
}

/// The models of every provider claw can reach now, for the `/model`
/// picker: each provider whose key is set (all of `/login`'s list), and the
/// local server when it answers. Fetched in parallel; a provider whose list
/// cannot be fetched is left out. Pairs of (provider label, model names
/// routed to it).
pub fn available_models() -> Vec<(&'static str, Vec<String>)> {
    let jobs = provider_choices()
        .into_iter()
        .filter_map(|choice| {
            let key = std::env::var(choice.key_env).unwrap_or_default();
            if key.trim().is_empty() && !choice.key_optional {
                return None;
            }
            let url = std::env::var(choice.url_env)
                .ok()
                .filter(|url| !url.trim().is_empty())
                .unwrap_or_else(|| choice.default_url.to_string());
            Some(std::thread::spawn(move || {
                let ids = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .ok()?
                    .block_on(api::list_models(&url, &key, choice.kind == "anthropic"))
                    .ok()?;
                let names = ids
                    .iter()
                    .map(|id| model_name_for(&choice, id))
                    .collect::<Vec<_>>();
                let label = if choice.kind == "local" {
                    "local server"
                } else {
                    choice.label
                };
                (!names.is_empty()).then_some((label, names))
            }))
        })
        .collect::<Vec<_>>();
    jobs.into_iter()
        .filter_map(|job| job.join().ok().flatten())
        .collect()
}

fn model_name_for(choice: &ProviderChoice, id: &str) -> String {
    match choice.kind {
        // DashScope routes by model family.
        "dashscope" if id.starts_with("kimi") => format!("kimi/{id}"),
        "dashscope" => format!("qwen/{id}"),
        _ => format!("{}{id}", choice.model_prefix),
    }
}

/// How many fetched models are listed; OpenRouter alone offers hundreds.
const MAX_LISTED_MODELS: usize = 60;

fn prompt_model(
    choice: &ProviderChoice,
    current: &RuntimeProviderConfig,
    same_provider: bool,
    active_model: Option<&str>,
    models: &[String],
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    let current_model = same_provider
        .then(|| current.model())
        .flatten()
        .filter(|model| !model.is_empty())
        .or(active_model)
        .map(str::to_string);
    let mut listed: Vec<String> = if models.is_empty() {
        choice
            .suggested_models
            .iter()
            .map(|model| prefixed_model(choice, model))
            .collect()
    } else {
        models.iter().take(MAX_LISTED_MODELS).cloned().collect()
    };
    if let Some(current) = &current_model {
        if !listed.contains(current) {
            listed.insert(0, current.clone());
        }
    }
    if listed.is_empty() {
        let typed = text("Model name:", false)?;
        return Ok((!typed.is_empty()).then(|| prefixed_model(choice, &typed)));
    }
    let options: Vec<(String, Option<String>)> = listed
        .iter()
        .map(|model| {
            let note = (current_model.as_ref() == Some(model)).then(|| "current".to_string());
            (model.clone(), note)
        })
        .collect();
    let default = current_model
        .as_ref()
        .and_then(|current| listed.iter().position(|model| model == current))
        .unwrap_or(0);
    let question = if choice.model_prefix.is_empty() || models.len() > MAX_LISTED_MODELS {
        format!(
            "Model ({} listed; \"Type something else\" for any other name)",
            listed.len()
        )
    } else {
        format!(
            "Model (or type a name; `{}` is added if missing)",
            choice.model_prefix
        )
    };
    match choose(&question, &options, default, true)? {
        Pick::Index(index) => Ok(Some(listed[index].clone())),
        Pick::Typed(name) => Ok(Some(prefixed_model(choice, &name))),
        Pick::Cancelled => Err(Box::new(Cancelled)),
    }
}

/// Adds the provider's prefix to a typed model name that lacks it, so it is
/// routed to the provider just configured.
fn prefixed_model(choice: &ProviderChoice, model: &str) -> String {
    let prefix = choice.model_prefix;
    // Aliases (opus, sonnet) and names already routed elsewhere stay as typed.
    if prefix.is_empty() || model.starts_with(prefix) {
        return model.to_string();
    }
    if choice.kind == "openai" && model.starts_with("gpt-") {
        return model.to_string();
    }
    format!("{prefix}{model}")
}

/// What to do with `subagentModel`.
enum FastModel {
    Keep,
    SameAsMain,
    Model(String),
}

fn prompt_fast_model(
    main_model: Option<&str>,
    models: &[String],
) -> Result<FastModel, Box<dyn std::error::Error>> {
    println!();
    println!("  A smaller, cheaper model can run the Agent tool's Explore, Plan and");
    println!("  Verification sub-agents to save tokens.");
    let current_fast = load_current_settings_field("subagentModel");
    let mut options: Vec<(String, Option<String>)> = Vec::new();
    if let Some(fast) = &current_fast {
        options.push((fast.clone(), Some("current; keep it".to_string())));
    }
    options.push((
        SAME_AS_MAIN.to_string(),
        main_model.map(|model| format!("sub-agents use {model}")),
    ));
    for model in models.iter().take(MAX_LISTED_MODELS) {
        if Some(model) != current_fast.as_ref() {
            options.push((model.clone(), None));
        }
    }
    match choose("Fast model for sub-agents", &options, 0, true)? {
        Pick::Index(index) => {
            let label = &options[index].0;
            Ok(if label == SAME_AS_MAIN {
                FastModel::SameAsMain
            } else if Some(label) == current_fast.as_ref() {
                FastModel::Keep
            } else {
                FastModel::Model(label.clone())
            })
        }
        Pick::Typed(name) => Ok(FastModel::Model(name)),
        Pick::Cancelled => Err(Box::new(Cancelled)),
    }
}

const SAME_AS_MAIN: &str = "Same as the main model";

fn load_current_settings_field(field: &str) -> Option<String> {
    let home = std::env::var("HOME").ok()?;
    let settings_path = std::path::Path::new(&home).join(".claw/settings.json");
    let content = std::fs::read_to_string(&settings_path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&content).ok()?;
    json.get(field)?.as_str().map(ToString::to_string)
}

pub(crate) fn save_settings_field(
    field: &str,
    value: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    save_settings_value(field, serde_json::Value::String(value.to_string()))
}

/// Sets `field` in `~/.claw/settings.json` to any JSON `value`.
pub(crate) fn save_settings_value(
    field: &str,
    value: serde_json::Value,
) -> Result<(), Box<dyn std::error::Error>> {
    let home = std::env::var("HOME")?;
    let settings_dir = std::path::Path::new(&home).join(".claw");
    let settings_path = settings_dir.join("settings.json");

    let mut settings: serde_json::Value = if settings_path.exists() {
        let content = std::fs::read_to_string(&settings_path)?;
        serde_json::from_str(&content)?
    } else {
        serde_json::json!({})
    };

    if let Some(obj) = settings.as_object_mut() {
        obj.insert(field.to_string(), value);
    }

    std::fs::create_dir_all(&settings_dir)?;
    std::fs::write(&settings_path, serde_json::to_string_pretty(&settings)?)?;
    Ok(())
}

fn remove_settings_field(field: &str) -> Result<(), Box<dyn std::error::Error>> {
    let home = std::env::var("HOME")?;
    let settings_path = std::path::Path::new(&home).join(".claw/settings.json");
    let Ok(content) = std::fs::read_to_string(&settings_path) else {
        return Ok(());
    };
    let mut settings: serde_json::Value = serde_json::from_str(&content)?;
    if let Some(obj) = settings.as_object_mut() {
        if obj.remove(field).is_some() {
            std::fs::write(&settings_path, serde_json::to_string_pretty(&settings)?)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{model_name_for, prefixed_model, provider_choices};

    fn choice(kind: &str) -> super::ProviderChoice {
        provider_choices()
            .into_iter()
            .find(|choice| choice.kind == kind)
            .expect("provider listed")
    }

    #[test]
    fn lists_every_supported_provider_once() {
        let kinds = provider_choices()
            .iter()
            .map(|choice| choice.kind)
            .collect::<Vec<_>>();
        for kind in [
            "anthropic",
            "openai",
            "zai",
            "openrouter",
            "deepseek",
            "gemini",
            "groq",
            "mistral",
            "xai",
            "dashscope",
            "local",
        ] {
            assert_eq!(kinds.iter().filter(|k| **k == kind).count(), 1, "{kind}");
        }
    }

    #[test]
    fn listed_and_typed_models_route_to_the_chosen_provider() {
        assert_eq!(model_name_for(&choice("zai"), "glm-4.6"), "zai/glm-4.6");
        assert_eq!(
            model_name_for(&choice("openrouter"), "anthropic/claude-sonnet-4"),
            "openrouter/anthropic/claude-sonnet-4"
        );
        assert_eq!(
            model_name_for(&choice("dashscope"), "kimi-k2.5"),
            "kimi/kimi-k2.5"
        );
        assert_eq!(
            model_name_for(&choice("dashscope"), "qwen-max"),
            "qwen/qwen-max"
        );
        assert_eq!(model_name_for(&choice("local"), "qwen3"), "local/qwen3");
        assert_eq!(prefixed_model(&choice("zai"), "glm-4.6"), "zai/glm-4.6");
        assert_eq!(prefixed_model(&choice("zai"), "zai/glm-4.6"), "zai/glm-4.6");
        assert_eq!(prefixed_model(&choice("anthropic"), "opus"), "opus");
        assert_eq!(prefixed_model(&choice("openai"), "gpt-5.4"), "gpt-5.4");
        assert_eq!(prefixed_model(&choice("openai"), "o3"), "openai/o3");
    }

    #[test]
    fn the_model_in_use_tells_the_current_provider() {
        let choices = provider_choices();
        let kind = |model| super::kind_for_model(model, &choices);
        assert_eq!(kind("local/qwen36-35b-a3b"), Some("local"));
        assert_eq!(kind("zai/glm-4.6"), Some("zai"));
        assert_eq!(
            kind("nvidia/qwen/qwen3-coder-480b-a35b-instruct"),
            Some("nvidia")
        );
        assert_eq!(kind("qwen/qwen-plus"), Some("dashscope"));
        assert_eq!(kind("gpt-5.4"), Some("openai"));
        assert_eq!(kind("anthropic/claude-opus-4-7"), Some("anthropic"));
        assert_eq!(kind("something-else"), None);
    }
}
