//! Stateful slash commands. Mutations preserve unrelated configuration and
//! reject malformed input before replacing a settings file.
use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{json, Value};

use crate::LiveCli;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
static TEMP_ID: AtomicU64 = AtomicU64::new(0);

fn local_settings() -> Result<PathBuf> {
    Ok(env::current_dir()?.join(".claw/settings.local.json"))
}

fn read_object(path: &Path) -> Result<Value> {
    let root = match fs::read_to_string(path) {
        Ok(text) => serde_json::from_str::<Value>(&text)
            .map_err(|e| format!("{}: {e}; file left unchanged", path.display()))?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => json!({}),
        Err(e) => return Err(e.into()),
    };
    if !root.is_object() {
        return Err(format!("{} must contain a JSON object", path.display()).into());
    }
    Ok(root)
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().ok_or("file has no parent directory")?;
    fs::create_dir_all(parent)?;
    let temp = parent.join(format!(
        ".claw-edit-{}-{}",
        std::process::id(),
        TEMP_ID.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| -> Result<()> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn write_object(path: &Path, value: &Value) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    atomic_write(path, &bytes)
}

fn set_nested(root: &mut Value, key: &str, value: Value) -> Result<()> {
    let keys: Vec<_> = key.split('.').collect();
    let mut object = root;
    for part in &keys[..keys.len() - 1] {
        let map = object
            .as_object_mut()
            .ok_or("setting parent is not an object")?;
        object = map.entry((*part).to_string()).or_insert_with(|| json!({}));
    }
    object
        .as_object_mut()
        .ok_or("setting parent is not an object")?
        .insert(keys[keys.len() - 1].to_string(), value);
    Ok(())
}

fn config_value(key: &str, value: &str) -> Result<Value> {
    match key {
        "theme" => {
            crate::render::ThemeName::parse(value)?;
            Ok(json!(value))
        }
        "model" if !value.is_empty() => Ok(json!(value)),
        "autoMemoryEnabled" | "sandbox.enabled" | "sandbox.networkIsolation" => match value {
            "true" => Ok(json!(true)),
            "false" => Ok(json!(false)),
            _ => Err(format!("{key} requires true or false").into()),
        },
        "permissions.defaultMode"
            if matches!(
                value,
                "read-only" | "workspace-write" | "danger-full-access"
            ) =>
        {
            Ok(json!(value))
        }
        "permissions.allow" | "permissions.ask" | "permissions.deny" => {
            let parsed: Value = serde_json::from_str(value)?;
            if !parsed
                .as_array()
                .is_some_and(|a| a.iter().all(Value::is_string))
            {
                return Err(format!("{key} requires a JSON array of strings").into());
            }
            Ok(parsed)
        }
        _ => Err(format!(
            "unsupported setting or value: {key}; /config --help lists supported keys"
        )
        .into()),
    }
}

fn apply_assignments(root: &Value, args: &str) -> Result<Value> {
    let mut result = root.clone();
    // Values are whitespace-free; use /config edit for strings/arrays containing spaces.
    let pairs: Vec<_> = args
        .split_whitespace()
        .map(|arg| {
            let (key, value) = arg.split_once('=').ok_or("expected key=value")?;
            Ok((key, config_value(key, value)?))
        })
        .collect::<Result<_>>()?;
    if pairs.is_empty() {
        return Err("expected at least one key=value".into());
    }
    for (key, value) in pairs {
        set_nested(&mut result, key, value)?;
    }
    Ok(result)
}

fn edit_file(path: &Path) -> Result<()> {
    if !io::stdin().is_terminal() {
        return Err(format!(
            "editing requires a terminal; edit {} directly",
            path.display()
        )
        .into());
    }
    let editor = env::var("VISUAL")
        .or_else(|_| env::var("EDITOR"))
        .map_err(|_| "set VISUAL or EDITOR to edit this file")?;
    if !path.exists() {
        atomic_write(path, b"")?;
    }
    // The editor is the user's command; the filename is passed separately,
    // never interpolated into shell syntax.
    let _pause = runtime::turn_keys::pause();
    let _screen = crate::claude_commands::external_terminal();
    #[cfg(not(windows))]
    let status = Command::new("sh")
        .arg("-c")
        .arg(format!("exec {editor} \"$@\""))
        .arg("claw-editor")
        .arg(path)
        .status()?;
    #[cfg(windows)]
    let status = Command::new(editor).arg(path).status()?;
    if !status.success() {
        return Err(format!("editor exited with {status}").into());
    }
    Ok(())
}

fn permission_edit(root: &mut Value, args: &str) -> Result<()> {
    let mut words = args.splitn(3, char::is_whitespace);
    let action = words.next().unwrap_or_default();
    let bucket = words.next().unwrap_or_default();
    let rule = words.next().unwrap_or_default().trim();
    if !matches!(action, "add" | "remove")
        || !matches!(bucket, "allow" | "ask" | "deny")
        || rule.is_empty()
    {
        return Err("usage: /permissions add|remove allow|ask|deny <rule>".into());
    }
    let permissions = root
        .as_object_mut()
        .ok_or("settings must be an object")?
        .entry("permissions")
        .or_insert_with(|| json!({}));
    let rules = permissions
        .as_object_mut()
        .ok_or("permissions must be an object")?
        .entry(bucket)
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .ok_or("permission rules must be an array")?;
    if action == "add" {
        if !rules.iter().any(|r| r.as_str() == Some(rule)) {
            rules.push(json!(rule));
        }
    } else {
        if !rules.iter().any(|r| r.as_str() == Some(rule)) {
            return Err("rule is not in project-local settings; edit the file that defines the inherited rule".into());
        }
        rules.retain(|r| r.as_str() != Some(rule));
    }
    Ok(())
}

impl LiveCli {
    pub(crate) fn run_local_action(&mut self, name: &str, args: Option<&str>) -> Result<bool> {
        let args = args.unwrap_or_default().trim();
        match name {
            "/vim" => {
                let on = match args { "" => !crate::input::vim_enabled(), "on" => true, "off" => false, _ => return Err("usage: /vim [on|off]".into()) };
                runtime::save_user_setting("editorMode", Some(if on { "vim" } else { "normal" }))?;
                crate::input::set_vim_enabled(on);
                println!("Editor mode: {}. Vim: Esc normal; i/a/I/A insert; h/j/k/l, w/b, 0/$, x.", if on { "vim" } else { "normal" });
            }
            "/upgrade" | "/usage-credits" | "/privacy-settings" => open_account_page(name)?,
            "/install-github-app" => {
                let remote = Command::new("git").args(["remote", "get-url", "origin"]).output()?;
                let remote = String::from_utf8_lossy(&remote.stdout);
                if !(remote.starts_with("https://github.com/") || remote.starts_with("git@github.com:") || remote.starts_with("ssh://git@github.com/")) {
                    return Err("/install-github-app requires an origin remote on github.com".into());
                }
                open_url("https://github.com/apps/claude/installations/new")?;
                println!("Complete repository selection and installation in GitHub. Workflow and secret setup remain separate: https://code.claude.com/docs/en/github-actions");
            }
            "/rate-limit-options" => match args {
                "" => println!("Wait for the provider's limit to reset, then retry your request.
/rate-limit-options usage opens Claude usage and credits.
/rate-limit-options upgrade opens plan upgrades.
API accounts manage limits with their API provider; this command does not change billing automatically."),
                "usage" => open_account_page("/usage-credits")?,
                "upgrade" => open_account_page("/upgrade")?,
                _ => return Err("usage: /rate-limit-options [usage|upgrade]".into()),
            },
            "/config" => self.edit_config(args)?,
            "/permissions" => self.edit_permissions(args)?,
            "/memory" => self.edit_memory(args)?,
            "/sandbox" => self.edit_sandbox(args)?,
            "/mcp" => self.edit_mcp(args)?,
            "/release-notes" => print!("{}", include_str!("../CHANGELOG.md")),
            "/terminal-setup" => install_terminal_binding()?,
            "/heapdump" => self.write_diagnostics()?,
            "/ide" => ide(args)?,
            "/artifacts" => self.local_artifacts(args)?,
            "/fast" => self.toggle_fast(args)?,
            "/login" => {
                // The provider, API key and model, saved to ~/.claw/settings.json.
                crate::setup_wizard::run_setup_wizard()?;
                self.refresh_settings()?;
            }
            "/logout" => {
                runtime::clear_user_provider_settings()?;
                println!("Removed the saved provider, key and model from ~/.claw/settings.json. Keys in environment variables (ANTHROPIC_API_KEY, OPENAI_API_KEY, …) still apply; unset them to sign out completely. /login sets a provider again.");
            }
            "/auto-mode-setup" => self.auto_mode_setup(args)?,
            "/schedule" => schedule(args)?,
            "/ultrareview" | "/autofix-pr" => {
                let prompt = agent_workflow_prompt(name, args);
                self.run_turn(&prompt)?;
                return Ok(true);
            }
            "/artifact-capabilities" | "/artifact-diagramming" | "/claude-api" | "/plugin-authoring" | "/design" | "/slides" => {
                let prompt = reference_prompt(name, args)?;
                self.run_turn(&prompt)?;
                return Ok(true);
            }
            _ => return Err(format!("unknown local action {name}").into()),
        }
        Ok(false)
    }

    /// `/fast [on|off]`: quicker replies. Claude Code's fast mode is a
    /// faster serving of the same model; locally that means low reasoning
    /// effort, plus the `fastModel` setting's model when one is set.
    fn toggle_fast(&mut self, args: &str) -> Result<()> {
        let mut fast = FAST_MODE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let on = match args {
            "" => fast.is_none(),
            "on" => true,
            "off" => false,
            _ => return Err("usage: /fast [on|off]".into()),
        };
        match (on, fast.is_some()) {
            (true, true) | (false, false) => {}
            (true, false) => {
                let previous_model = self.model.clone();
                *fast = Some((self.reasoning_effort.clone(), previous_model.clone()));
                self.set_reasoning_effort(Some("low".to_string()));
                if let Some(model) =
                    runtime::read_user_setting("fastModel").filter(|m| *m != previous_model)
                {
                    self.set_model(Some(model))?;
                }
            }
            (false, true) => {
                if let Some((effort, model)) = fast.take() {
                    self.set_reasoning_effort(effort);
                    if model != self.model {
                        self.set_model(Some(model))?;
                    }
                }
            }
        }
        let state = if fast.is_some() { "on" } else { "off" };
        println!(
            "Fast mode {state}: reasoning effort {}, model {}.{}",
            self.reasoning_effort.as_deref().unwrap_or("model default"),
            self.model,
            if fast.is_some() && runtime::read_user_setting("fastModel").is_none() {
                "\n  Set \"fastModel\" in ~/.claw/settings.json to also switch to a quicker model."
            } else {
                ""
            }
        );
        Ok(())
    }

    /// `/auto-mode-setup [careful|balanced|autonomous]`: how much runs
    /// without asking. Claude Code's auto mode uses a hosted classifier;
    /// here a preset sets the permission mode, bash approval and rules in
    /// `.claw/settings.local.json`.
    fn auto_mode_setup(&mut self, args: &str) -> Result<()> {
        let path = local_settings()?;
        let preset = match args {
            "" => {
                let config = runtime::ConfigLoader::default_for(env::current_dir()?).load()?;
                println!(
                    "Auto-approval presets (saved to {}):\n  careful     workspace-write; every shell command asks\n  balanced    workspace-write; only risky shell commands ask; common read-only and build/test commands allowed\n  autonomous  full access; only risky shell commands ask; destructive commands denied\nCurrent rules:",
                    path.display()
                );
                print!("{}", render_permission_rules(config.permission_rules()));
                println!("Usage: /auto-mode-setup careful|balanced|autonomous");
                return Ok(());
            }
            other => AutoPreset::parse(other)
                .ok_or("usage: /auto-mode-setup [careful|balanced|autonomous]")?,
        };
        let mut root = read_object(&path)?;
        set_nested(&mut root, "permissions.defaultMode", json!(preset.mode()))?;
        set_nested(
            &mut root,
            "permissions.bashApproval",
            json!(preset.bash_approval()),
        )?;
        for (bucket, rules) in [("allow", preset.allow()), ("deny", preset.deny())] {
            let mut current: Vec<Value> = root
                .pointer(&format!("/permissions/{bucket}"))
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            for rule in rules {
                if !current.iter().any(|r| r.as_str() == Some(rule)) {
                    current.push(json!(rule));
                }
            }
            set_nested(
                &mut root,
                &format!("permissions.{bucket}"),
                Value::Array(current),
            )?;
        }
        write_object(&path, &root)?;
        self.refresh_settings()?;
        println!(
            "Auto-approval set to {args} in {} (mode {}, shell approval: {}). It applies from the next request; /permissions list shows the rules.",
            path.display(),
            preset.mode(),
            preset.bash_approval()
        );
        Ok(())
    }

    fn refresh_settings(&mut self) -> Result<()> {
        self.system_prompt = crate::build_system_prompt(&self.model)?;
        self.rebuild_runtime()
    }

    fn edit_config(&mut self, args: &str) -> Result<()> {
        if args == "--help" {
            println!("/config key=value ... (project-local settings)\nKeys: model, theme, autoMemoryEnabled, sandbox.enabled, sandbox.networkIsolation, permissions.defaultMode, permissions.allow, permissions.ask, permissions.deny.\nBoolean values: true/false. Rule lists: JSON arrays without spaces. /config edit opens the file for complex values.");
            return Ok(());
        }
        let path = local_settings()?;
        if args == "edit" {
            if !path.exists() {
                write_object(&path, &json!({}))?;
            }
            edit_file(&path)?;
            read_object(&path)?;
        } else {
            let root = apply_assignments(&read_object(&path)?, args)?;
            write_object(&path, &root)?;
        }
        let config = runtime::ConfigLoader::default_for(env::current_dir()?).load()?;
        if let Some(theme) = config.get("theme").and_then(|v| v.as_str()) {
            crate::render::set_current_theme(crate::render::ThemeName::parse(theme)?);
        }
        if args == "edit"
            || args
                .split_whitespace()
                .any(|s| s.starts_with("permissions.defaultMode="))
        {
            if let Some(mode) = config.permission_mode() {
                self.apply_permission_mode(match mode {
                    runtime::ResolvedPermissionMode::ReadOnly => runtime::PermissionMode::ReadOnly,
                    runtime::ResolvedPermissionMode::WorkspaceWrite => {
                        runtime::PermissionMode::WorkspaceWrite
                    }
                    runtime::ResolvedPermissionMode::DangerFullAccess => {
                        runtime::PermissionMode::DangerFullAccess
                    }
                })?;
            }
        }
        if args == "edit" || args.split_whitespace().any(|s| s.starts_with("model=")) {
            if let Some(model) = config.model() {
                self.set_model(Some(model.to_string()))?;
            }
        }
        self.refresh_settings()?;
        println!("Settings saved to {} and applied.", path.display());
        Ok(())
    }

    fn edit_permissions(&mut self, args: &str) -> Result<()> {
        let path = local_settings()?;
        if args == "list" || args.is_empty() {
            let config = runtime::ConfigLoader::default_for(env::current_dir()?).load()?;
            print!("{}", render_permission_rules(config.permission_rules()));
            return Ok(());
        }
        let mut root = read_object(&path)?;
        // Rules are additive across scopes. Only remove project-local rules;
        // never pretend that a local list overrides a rule from another scope.
        permission_edit(&mut root, args)?;
        write_object(&path, &root)?;
        self.refresh_settings()?;
        println!(
            "Permission rules saved and applied from {}.",
            path.display()
        );
        Ok(())
    }

    fn edit_memory(&mut self, args: &str) -> Result<()> {
        match args {
            "" | "list" => {
                Self::print_memory()?;
                println!("/memory edit [project|user|auto], /memory on, /memory off");
                return Ok(());
            }
            "on" | "off" => {
                let path = local_settings()?;
                let mut root = read_object(&path)?;
                set_nested(&mut root, "autoMemoryEnabled", json!(args == "on"))?;
                write_object(&path, &root)?;
            }
            "edit" | "edit project" => edit_file(&env::current_dir()?.join("CLAUDE.md"))?,
            "edit user" => edit_file(&runtime::default_config_home().join("CLAUDE.md"))?,
            "edit auto" => edit_file(&runtime::memory_dir(&env::current_dir()?).join("MEMORY.md"))?,
            _ => return Err("usage: /memory [list|on|off|edit [project|user|auto]]".into()),
        }
        self.refresh_settings()?;
        Self::print_memory()?;
        Ok(())
    }

    fn edit_sandbox(&mut self, args: &str) -> Result<()> {
        // Bare /sandbox only reports: changing isolation must be explicit.
        if args.is_empty() || args == "status" {
            Self::print_sandbox_status();
            return Ok(());
        }
        let config = runtime::ConfigLoader::default_for(env::current_dir()?).load()?;
        let status = runtime::resolve_sandbox_status(config.sandbox(), &env::current_dir()?);
        let enabled = match args {
            "toggle" => !status.enabled,
            "on" => true,
            "off" => false,
            _ => return Err("usage: /sandbox [status|on|off|toggle]".into()),
        };
        let path = local_settings()?;
        let mut root = read_object(&path)?;
        set_nested(&mut root, "sandbox.enabled", json!(enabled))?;
        write_object(&path, &root)?;
        self.refresh_settings()?;
        Self::print_sandbox_status();
        Ok(())
    }

    fn edit_mcp(&mut self, args: &str) -> Result<()> {
        let parts: Vec<_> = args.split_whitespace().collect();
        let (action, target) = match parts.as_slice() {
            [action] => (*action, "all"),
            [action, target] => (*action, *target),
            _ => return Err("usage: /mcp enable|disable|reconnect [server|all]".into()),
        };
        if !matches!(action, "enable" | "disable" | "reconnect") {
            return Err("invalid MCP action".into());
        }
        let config = runtime::ConfigLoader::default_for(env::current_dir()?).load()?;
        let names: Vec<_> = config.mcp().servers().keys().cloned().collect();
        if target != "all" && !names.iter().any(|n| n == target) {
            return Err(format!("unknown MCP server: {target}").into());
        }
        if action != "reconnect" {
            let path = local_settings()?;
            let mut root = read_object(&path)?;
            let states = root
                .as_object_mut()
                .ok_or("settings must be an object")?
                .entry("mcpServerStates")
                .or_insert_with(|| json!({}))
                .as_object_mut()
                .ok_or("mcpServerStates must be an object")?;
            for name in names
                .iter()
                .filter(|n| target == "all" || n.as_str() == target)
            {
                states.insert(name.clone(), json!(action == "enable"));
            }
            write_object(&path, &root)?;
        }
        self.refresh_settings()?;
        println!(
            "MCP {action} applied to {target}; runtime connections and tool discovery refreshed."
        );
        Self::print_mcp(None, crate::CliOutputFormat::Text)?;
        Ok(())
    }

    fn write_diagnostics(&self) -> Result<()> {
        let path = runtime::default_config_home()
            .join("diagnostics")
            .join(format!(
                "{}-{}.json",
                std::process::id(),
                TEMP_ID.fetch_add(1, Ordering::Relaxed)
            ));
        let process = fs::read_to_string("/proc/self/status").ok().map(|text| {
            text.lines()
                .filter(|l| ["Vm", "Rss", "Threads:"].iter().any(|p| l.starts_with(p)))
                .map(String::from)
                .collect::<Vec<_>>()
        });
        write_object(
            &path,
            &json!({"kind":"rust_memory_diagnostics", "pid":std::process::id(), "messages":self.runtime.session().messages.len(), "estimated_context_tokens":runtime::estimate_session_tokens(self.runtime.session()), "process_memory":process}),
        )?;
        println!("Rust memory diagnostics: {} (no conversation text or credentials; no JavaScript heap in this process)", path.display());
        Ok(())
    }

    fn local_artifacts(&mut self, args: &str) -> Result<()> {
        let root = env::current_dir()?.join(".claw/artifacts");
        if args.is_empty() || args == "list" {
            if !root.exists() {
                println!(
                    "No local artifacts. /design or /slides creates one in {}.",
                    root.display()
                );
                return Ok(());
            }
            let mut entries = fs::read_dir(&root)?.collect::<io::Result<Vec<_>>>()?;
            entries.sort_by_key(|e| e.file_name());
            for entry in entries {
                if entry.file_type()?.is_file() {
                    println!("{}", entry.path().display());
                }
            }
            return Ok(());
        }
        let (action, relative) = args
            .split_once(' ')
            .ok_or("usage: /artifacts [list|open <file>|attach <file>]")?;
        let path = contained_artifact(&root, relative.trim())?;
        match action {
            "open" => open_path(&path)?,
            "attach" => {
                let metadata = fs::metadata(&path)?;
                if metadata.len() > 1024 * 1024 {
                    return Err("artifact exceeds the 1 MiB text attachment limit".into());
                }
                let text = fs::read_to_string(&path)?;
                self.runtime
                    .session_mut()
                    .push_user_text(format!("Local artifact {}:\n{text}", path.display()))?;
                self.persist_session()?;
                println!("Attached {} to the conversation.", path.display());
            }
            _ => return Err("usage: /artifacts [list|open <file>|attach <file>]".into()),
        }
        Ok(())
    }
}

fn render_permission_rules(rules: &runtime::RuntimePermissionRuleConfig) -> String {
    let mut out = String::from("Effective permission rules (all settings scopes)\n");
    for (label, list) in [
        ("allow", rules.allow()),
        ("ask", rules.ask()),
        ("deny", rules.deny()),
        ("denied tools", rules.denied_tools()),
    ] {
        out.push_str(&format!("  {label}\n"));
        if list.is_empty() {
            out.push_str("    (none)\n");
        }
        for rule in list {
            out.push_str(&format!("    {rule}\n"));
        }
    }
    out.push_str("Edit project-local rules with /permissions add|remove allow|ask|deny <rule>.\n");
    out
}

pub(crate) fn disabled_servers(
    config: &runtime::RuntimeConfig,
) -> std::collections::BTreeSet<String> {
    config
        .get("mcpServerStates")
        .and_then(|v| v.as_object())
        .map(|states| {
            states
                .iter()
                .filter(|(_, state)| state.as_bool() == Some(false))
                .map(|(name, _)| name.clone())
                .collect()
        })
        .unwrap_or_default()
}

fn contained_artifact(root: &Path, relative: &str) -> Result<PathBuf> {
    let root = root.canonicalize()?;
    let path = root.join(relative).canonicalize()?;
    if !path.starts_with(&root) || !path.is_file() {
        return Err("artifact must be a file inside .claw/artifacts".into());
    }
    Ok(path)
}

fn open_path(path: &Path) -> Result<()> {
    #[cfg(target_os = "macos")]
    let mut command = Command::new("open");
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut c = Command::new("explorer.exe");
        c.arg(path);
        c
    };
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let mut command = Command::new("xdg-open");
    #[cfg(not(target_os = "windows"))]
    command.arg(path);
    if !command.status()?.success() {
        return Err("could not open artifact".into());
    }
    Ok(())
}

fn ide(args: &str) -> Result<()> {
    match args.split_whitespace().collect::<Vec<_>>().as_slice() {
        [] | ["status"] => println!(
            "Terminal: {}\nWorkspace: {}\nUse /ide open code|cursor|zed to open this workspace.",
            env::var("TERM_PROGRAM").unwrap_or_else(|_| "unknown".into()),
            env::current_dir()?.display()
        ),
        ["open"] => {
            launch_editor("code")?;
        }
        ["open", program @ ("code" | "cursor" | "zed")] => {
            launch_editor(program)?;
        }
        _ => return Err("usage: /ide [status|open [code|cursor|zed]]".into()),
    }
    Ok(())
}
fn launch_editor(program: &str) -> Result<()> {
    if !Command::new(program)
        .arg(env::current_dir()?)
        .status()?
        .success()
    {
        return Err("editor failed to open workspace".into());
    }
    Ok(())
}

fn install_terminal_binding() -> Result<()> {
    let program = env::var("TERM_PROGRAM").unwrap_or_default();
    if program != "vscode" {
        return Err("automatic setup currently supports VS Code/Cursor terminals (TERM_PROGRAM=vscode); other terminals: use Alt+Enter or Ctrl+J for a newline".into());
    }
    let home = env::var_os("HOME").ok_or("HOME is not set")?;
    #[cfg(target_os = "macos")]
    let config = PathBuf::from(&home).join("Library/Application Support");
    #[cfg(target_os = "windows")]
    let config = PathBuf::from(env::var_os("APPDATA").ok_or("APPDATA is not set")?);
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let config = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(home).join(".config"));
    let editor = if env::var("TERM_PROGRAM_VERSION")
        .unwrap_or_default()
        .to_ascii_lowercase()
        .contains("cursor")
        || env::var_os("CURSOR_TRACE_ID").is_some()
    {
        "Cursor"
    } else {
        "Code"
    };
    let path = config.join(editor).join("User/keybindings.json");
    install_binding(&path)?;
    println!(
        "Installed Shift+Enter newline binding in {}.",
        path.display()
    );
    Ok(())
}

fn install_binding(path: &Path) -> Result<()> {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => "[]".into(),
        Err(e) => return Err(e.into()),
    };
    let mut bindings: Vec<Value> = serde_json::from_str(&text).map_err(|e| {
        format!(
            "{}: {e}; file left unchanged (JSONC comments are not supported)",
            path.display()
        )
    })?;
    let binding = json!({"key":"shift+enter", "command":"workbench.action.terminal.sendSequence", "args":{"text":"\u{1b}\r"}, "when":"terminalFocus"});
    if bindings.contains(&binding) {
        return Ok(());
    }
    if bindings
        .iter()
        .any(|b| b["key"] == "shift+enter" && b["when"] == "terminalFocus")
    {
        return Err("an existing Shift+Enter terminal binding conflicts; edit it manually".into());
    }
    bindings.push(binding);
    if path.exists() {
        let backup = path.with_extension("json.claw-backup");
        if !backup.exists() {
            atomic_write(&backup, text.as_bytes())?;
        }
    }
    atomic_write(path, &serde_json::to_vec_pretty(&bindings)?)
}

/// `/fast`'s saved state: the effort and model to go back to.
static FAST_MODE: std::sync::Mutex<Option<(Option<String>, String)>> = std::sync::Mutex::new(None);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AutoPreset {
    Careful,
    Balanced,
    Autonomous,
}

impl AutoPreset {
    fn parse(name: &str) -> Option<Self> {
        match name {
            "careful" | "strict" => Some(Self::Careful),
            "balanced" | "default" => Some(Self::Balanced),
            "autonomous" | "auto" | "permissive" => Some(Self::Autonomous),
            _ => None,
        }
    }

    fn mode(self) -> &'static str {
        match self {
            Self::Careful | Self::Balanced => "workspace-write",
            Self::Autonomous => "danger-full-access",
        }
    }

    fn bash_approval(self) -> &'static str {
        match self {
            Self::Careful => "always",
            Self::Balanced | Self::Autonomous => "risky",
        }
    }

    fn allow(self) -> &'static [&'static str] {
        match self {
            Self::Careful => &[],
            Self::Balanced | Self::Autonomous => &[
                "bash(git status*)",
                "bash(git diff*)",
                "bash(git log*)",
                "bash(git show*)",
                "bash(ls*)",
                "bash(cargo build*)",
                "bash(cargo check*)",
                "bash(cargo test*)",
                "bash(cargo clippy*)",
                "bash(npm test*)",
                "bash(npm run build*)",
                "bash(pytest*)",
                "bash(go test*)",
                "bash(make*)",
            ],
        }
    }

    fn deny(self) -> &'static [&'static str] {
        match self {
            Self::Careful | Self::Balanced => &[],
            Self::Autonomous => &[
                "bash(rm -rf /*)",
                "bash(rm -rf ~*)",
                "bash(git push --force*)",
                "bash(git push -f*)",
                "bash(git reset --hard*)",
                "bash(git clean -fd*)",
                "bash(sudo *)",
                "bash(mkfs*)",
                "bash(dd if=*)",
            ],
        }
    }
}

/// `/ultrareview` and `/autofix-pr`: Claude Code runs these in the cloud;
/// here the agent does the same work with local sub-agents and `gh`.
fn agent_workflow_prompt(name: &str, args: &str) -> String {
    let target = if args.is_empty() {
        "the current branch's changes"
    } else {
        args
    };
    match name {
        "/ultrareview" => format!(
            "Run a thorough multi-agent code review of {target} (a PR number via `gh pr diff`, a branch against its merge base with the default branch, or a path; with nothing named, the uncommitted and branch changes).\n\n\
             1. Work out the exact diff under review and summarize its scope.\n\
             2. Start four reviewers in parallel with the Agent tool (run_in_background: true), each given the full diff and one focus: correctness and edge cases; security (injection, auth, secrets, unsafe input); performance and resource use; tests and error handling. Each reports concrete defects with file, line, and a failure scenario (inputs or state -> wrong result), not style notes.\n\
             3. When all have reported, verify every finding yourself against the code; drop what you cannot confirm and mark the rest CONFIRMED or PLAUSIBLE.\n\
             4. Report the verified findings, most severe first, with the ReportFindings tool (an empty list if none survived), then a two-line summary. Do not change any files."
        ),
        _ => format!(
            "Fix the failing checks of the pull request for {target} (the PR of the current branch unless a PR number is given), using the GitHub CLI.\n\n\
             1. Find the PR (`gh pr view --json number,headRefName,url`) and its checks (`gh pr checks`). If gh is missing or not authenticated, say so and stop.\n\
             2. For each failing check, read the failed log (`gh run view <run-id> --log-failed`) and find the root cause.\n\
             3. Make sure the PR branch is checked out locally, fix the causes with minimal changes, and run the same commands locally until they pass.\n\
             4. Commit the fix with a message naming the checks fixed. Before pushing, ask the user with AskUserQuestion; push only if they agree.\n\
             5. Report what failed, what you changed, and the local verification output."
        ),
    }
}

// ───────────────────────────── /schedule ─────────────────────────────

/// Marks claw's lines in the user's crontab.
const SCHEDULE_TAG: &str = "# claw-schedule:";

/// Environment variables copied into scheduled runs. No secrets: API keys
/// stay in ~/.claw/settings.json or the user's own profile.
const SCHEDULE_ENV: &[&str] = &[
    "CLAW_LOCAL_BASE_URL",
    "OPENAI_BASE_URL",
    "CLAW_CONTEXT_WINDOW",
    "CLAW_TOOL_RESULT_MAX_CHARS",
    "CLAW_THEME",
];

fn read_crontab() -> Result<String> {
    let output = Command::new("crontab")
        .arg("-l")
        .output()
        .map_err(|e| format!("crontab is not available: {e}"))?;
    // `crontab -l` fails when the user has none yet.
    Ok(if output.status.success() {
        String::from_utf8_lossy(&output.stdout).to_string()
    } else {
        String::new()
    })
}

fn write_crontab(text: &str) -> Result<()> {
    let mut child = Command::new("crontab")
        .arg("-")
        .stdin(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("crontab is not available: {e}"))?;
    child
        .stdin
        .as_mut()
        .ok_or("crontab: no stdin")?
        .write_all(text.as_bytes())?;
    if !child.wait()?.success() {
        return Err("crontab rejected the new table; nothing changed".into());
    }
    Ok(())
}

fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// claw's scheduled entries: (id, cron line).
fn scheduled_entries(table: &str) -> Vec<(String, String)> {
    let lines: Vec<&str> = table.lines().collect();
    lines
        .iter()
        .enumerate()
        .filter_map(|(index, line)| {
            let id = line.strip_prefix(SCHEDULE_TAG)?.trim().to_string();
            let entry = (*lines.get(index + 1)?).to_string();
            Some((id, entry))
        })
        .collect()
}

fn schedule_log_dir() -> PathBuf {
    runtime::default_config_home().join("schedules")
}

/// `/schedule [list|add <cron> <prompt>|remove <id>|run <id>]`: routines
/// that run `claw -p <prompt>` from cron in this directory, logging to
/// ~/.claw/schedules/<id>.log. Claude Code's routines run on claude.ai;
/// these run on this machine, while it is on.
fn schedule(args: &str) -> Result<()> {
    let (action, rest) = args.split_once(char::is_whitespace).unwrap_or((args, ""));
    let rest = rest.trim();
    match action {
        "" | "list" => {
            let entries = scheduled_entries(&read_crontab()?);
            if entries.is_empty() {
                println!("No scheduled routines. /schedule add \"0 9 * * 1-5\" <prompt> adds one.");
            }
            for (id, line) in entries {
                let log = schedule_log_dir().join(format!("{id}.log"));
                let last = fs::metadata(&log)
                    .and_then(|meta| meta.modified())
                    .ok()
                    .map_or_else(
                        || "not run yet".to_string(),
                        |time| {
                            let age = time.elapsed().map(|d| d.as_secs() / 60).unwrap_or(0);
                            format!("last run {age} min ago")
                        },
                    );
                let timing = if line.starts_with('@') { 1 } else { 5 };
                let when: String = line
                    .split_whitespace()
                    .take(timing)
                    .collect::<Vec<_>>()
                    .join(" ");
                println!("{id}  {when}  ({last}; log {})", log.display());
            }
        }
        "add" => {
            let (expression, prompt) = split_cron(rest).ok_or(
                "usage: /schedule add <cron> <prompt>   e.g. /schedule add \"0 9 * * 1-5\" summarize yesterday's commits",
            )?;
            runtime::scheduler::CronSchedule::parse(&expression)?;
            let table = read_crontab()?;
            let next = scheduled_entries(&table)
                .iter()
                .filter_map(|(id, _)| id.strip_prefix('s').and_then(|n| n.parse::<u32>().ok()))
                .max()
                .unwrap_or(0)
                + 1;
            let id = format!("s{next}");
            fs::create_dir_all(schedule_log_dir())?;
            let log = schedule_log_dir().join(format!("{id}.log"));
            let exe = env::current_exe()?;
            let env_part: String = SCHEDULE_ENV
                .iter()
                .filter_map(|name| {
                    env::var(name)
                        .ok()
                        .map(|value| format!("{name}={} ", shell_quote(&value)))
                })
                .collect();
            let line = format!(
                "{expression} cd {} && {env_part}{} -p {} --permission-mode workspace-write >> {} 2>&1",
                shell_quote(&env::current_dir()?.display().to_string()),
                shell_quote(&exe.display().to_string()),
                // `%` ends a crontab command line unless escaped.
                shell_quote(&prompt).replace('%', "\\%"),
                shell_quote(&log.display().to_string()),
            );
            let mut table = table;
            if !table.is_empty() && !table.ends_with('\n') {
                table.push('\n');
            }
            table.push_str(&format!("{SCHEDULE_TAG}{id}\n{line}\n"));
            write_crontab(&table)?;
            println!(
                "Scheduled {id} ({expression}): runs `claw -p` in this directory with workspace-write permissions while this machine is on; output goes to {}. /schedule remove {id} deletes it.",
                log.display()
            );
        }
        "remove" | "delete" => {
            let table = read_crontab()?;
            if !scheduled_entries(&table).iter().any(|(id, _)| id == rest) {
                return Err(
                    format!("no scheduled routine `{rest}` (/schedule list shows them)").into(),
                );
            }
            let mut kept = Vec::new();
            let mut skip = false;
            for line in table.lines() {
                if skip {
                    skip = false;
                    continue;
                }
                if line.strip_prefix(SCHEDULE_TAG).map(str::trim) == Some(rest) {
                    skip = true;
                    continue;
                }
                kept.push(line);
            }
            let mut text = kept.join("\n");
            if !text.is_empty() {
                text.push('\n');
            }
            write_crontab(&text)?;
            println!("Removed {rest}.");
        }
        "run" => {
            let entries = scheduled_entries(&read_crontab()?);
            let (_, line) = entries
                .iter()
                .find(|(id, _)| id == rest)
                .ok_or_else(|| format!("no scheduled routine `{rest}`"))?;
            let timing = if line.starts_with('@') { 1 } else { 5 };
            // Everything after the schedule is the command.
            let command: String = line
                .split_whitespace()
                .skip(timing)
                .collect::<Vec<_>>()
                .join(" ")
                .replace("\\%", "%");
            let mut shell = Command::new("sh");
            shell.arg("-c").arg(&command);
            let job =
                runtime::global_background_jobs().start(shell, &format!("/schedule run {rest}"))?;
            println!("Started {rest} now as background job {job} (/tasks shows it).");
        }
        _ => return Err("usage: /schedule [list|add <cron> <prompt>|remove <id>|run <id>]".into()),
    }
    Ok(())
}

/// `"0 9 * * 1-5" prompt…`, `0 9 * * 1-5 prompt…` or `@daily prompt…` →
/// (expression, prompt).
fn split_cron(text: &str) -> Option<(String, String)> {
    let text = text.trim();
    if let Some(quoted) = text.strip_prefix('"') {
        let (expression, prompt) = quoted.split_once('"')?;
        let prompt = prompt.trim();
        return (!prompt.is_empty()).then(|| (expression.trim().to_string(), prompt.to_string()));
    }
    let fields = if text.starts_with('@') { 1 } else { 5 };
    let mut rest = text;
    let mut expression = Vec::new();
    for _ in 0..fields {
        let (field, after) = rest.split_once(char::is_whitespace)?;
        expression.push(field);
        rest = after.trim_start();
    }
    (!rest.is_empty()).then(|| (expression.join(" "), rest.to_string()))
}

fn reference_prompt(name: &str, args: &str) -> Result<String> {
    let guidance = match name {
        "/artifact-capabilities" => "Explain and use Claw's local artifacts: save HTML, SVG, Markdown, or office files under .claw/artifacts; /artifacts lists them, opens them with a local application, or attaches UTF-8 text to the conversation. Inspect available tools before choosing a format. Browser HTML has normal browser security restrictions and cannot call MCP connectors directly. Do not claim hosting, sharing permissions, or a remote artifact API exists.",
        "/artifact-diagramming" => "Create legible diagrams as self-contained SVG in .claw/artifacts. Use a viewBox, title and description, labeled connectors, generous spacing, readable text, semantic grouping, keyboard-accessible links, and colors with contrast on light and dark backgrounds. Prefer a simple diagram answering one concrete question. Check overlapping labels and clipped arrows; report the output file.",
        "/claude-api" => "Help with the Anthropic API task below. Inspect the project's language and installed SDK version. Fetch the official documentation at https://platform.claude.com/docs/en/api/overview and follow its relevant official links before selecting current models, SDK methods, authentication, streaming, tools, or managed-agent features. Never invent APIs. Preserve secrets in environment variables. Implement and test the requested integration, or explain the verified API with runnable examples if no implementation was requested.",
        "/plugin-authoring" => "Build or improve a Claw plugin for the request below. First read the local plugin loader, manifest schema, and existing tests/examples (rust/crates/plugins when present). Plugins use .claude-plugin/plugin.json. Inspect actual supported command, skill, agent, hook and MCP discovery behavior before generating files; do not assume an upstream field is supported. Create the manifest and resources in a separate plugin directory, validate loading with Claw's plugin command, and report installation steps. Do not install or execute third-party code without the user's instruction.",
        "/design" if !args.is_empty() => "Create a polished local design artifact for the brief below as a self-contained HTML file with inline CSS and SVG in .claw/artifacts. Use multiple clearly labeled artboards if helpful, responsive layout, accessible labels and contrast, and print CSS. Inspect existing project styles first. Render/inspect with available browser tools if possible, fix layout issues, and report the local file path. This is a local artifact, not a hosted Claude Design canvas.",
        "/slides" if !args.is_empty() => "Create a presentation for the brief below in .claw/artifacts. Inspect the available document tools; use a PowerPoint writer if available, otherwise create a self-contained HTML slide deck with keyboard previous/next navigation and printable slides. Develop a coherent narrative with concise slide text and readable diagrams. Verify the generated file with available tools and report its path. Do not claim it is hosted in Claude Slides.",
        "/design" | "/slides" => return Err(format!("usage: {name} <brief>").into()),
        _ => return Err("unknown reference command".into()),
    };
    Ok(format!("{guidance}\n\nUser request: {args}"))
}

// Official account destinations; no credentials or session content in URLs.
fn account_url(name: &str) -> Option<&'static str> {
    match name {
        "/upgrade" => Some("https://claude.ai/upgrade"),
        "/usage-credits" => Some("https://claude.ai/settings/usage"),
        "/privacy-settings" => Some("https://claude.ai/settings/data-privacy-controls"),
        _ => None,
    }
}
fn open_account_page(name: &str) -> Result<()> {
    open_url(account_url(name).ok_or("unknown account page")?)
}
fn open_url(url: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    let mut command = Command::new("open");
    #[cfg(target_os = "windows")]
    let mut command = Command::new("explorer.exe");
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let mut command = Command::new("xdg-open");
    match command.arg(url).status() {
        Ok(status) if status.success() => println!("Opened {url}"),
        _ => println!("Open this page in your browser: {url}"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            let path = env::temp_dir().join(format!(
                "claw-actions-{}-{}",
                std::process::id(),
                TEMP_ID.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn schedule_parses_cron_and_finds_its_entries() {
        assert_eq!(
            split_cron("\"0 9 * * 1-5\" summarize commits"),
            Some(("0 9 * * 1-5".to_string(), "summarize commits".to_string()))
        );
        assert_eq!(
            split_cron("*/5 * * * * check the build"),
            Some(("*/5 * * * *".to_string(), "check the build".to_string()))
        );
        assert_eq!(
            split_cron("@daily tidy up"),
            Some(("@daily".to_string(), "tidy up".to_string()))
        );
        assert_eq!(split_cron("0 9 * *"), None);
        assert_eq!(split_cron("0 9 * * *"), None, "no prompt");
        let table =
            "MAILTO=me\n# claw-schedule:s1\n0 9 * * * cd '/x' && claw -p 'a'\n* * * * * other\n";
        let entries = scheduled_entries(table);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, "s1");
        assert!(entries[0].1.starts_with("0 9 * * *"));
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn auto_presets_are_consistent() {
        let balanced = AutoPreset::parse("balanced").unwrap();
        assert_eq!(balanced.mode(), "workspace-write");
        assert_eq!(balanced.bash_approval(), "risky");
        assert!(balanced.deny().is_empty());
        let autonomous = AutoPreset::parse("autonomous").unwrap();
        assert_eq!(autonomous.mode(), "danger-full-access");
        assert!(autonomous
            .deny()
            .iter()
            .any(|rule| rule.contains("push --force")));
        assert_eq!(AutoPreset::parse("nope"), None);
    }

    #[test]
    fn workflow_prompts_name_their_target() {
        let review = agent_workflow_prompt("/ultrareview", "123");
        assert!(review.contains("of 123") && review.contains("ReportFindings"));
        let fix = agent_workflow_prompt("/autofix-pr", "");
        assert!(fix.contains("current branch") && fix.contains("gh pr checks"));
    }

    #[test]
    fn config_batch_validates_before_mutating_and_preserves_other_keys() {
        let original = json!({"provider":{"kind":"ollama"}, "sandbox":{"networkIsolation":true}});
        assert!(apply_assignments(&original, "theme=light autoMemoryEnabled=maybe").is_err());
        let changed = apply_assignments(
            &original,
            "theme=light autoMemoryEnabled=false sandbox.enabled=true",
        )
        .unwrap();
        assert_eq!(changed["provider"], original["provider"]);
        assert_eq!(
            changed["sandbox"],
            json!({"enabled":true,"networkIsolation":true})
        );
        assert_eq!(changed["autoMemoryEnabled"], false);
        assert!(original.get("theme").is_none());
        assert!(apply_assignments(&original, "permissions.defaultMode=dontAsk").is_err());
        assert!(apply_assignments(&original, "unknown=true").is_err());
        assert!(apply_assignments(&original, "theme=not-a-theme").is_err());
        assert!(apply_assignments(&original, "permissions.allow=5").is_err());
    }

    #[test]
    fn writes_preserve_invalid_files_and_round_trip_typed_settings() {
        let temp = Temp::new();
        let path = temp.0.join(".claw/settings.local.json");
        let value = apply_assignments(
            &json!({}),
            "autoMemoryEnabled=false sandbox.enabled=true permissions.defaultMode=read-only",
        )
        .unwrap();
        write_object(&path, &value).unwrap();
        let config = runtime::ConfigLoader::new(&temp.0, temp.0.join("config"))
            .load()
            .unwrap();
        assert!(!runtime::memory_enabled(&config));
        assert_eq!(config.sandbox().enabled, Some(true));
        assert_eq!(
            config.permission_mode(),
            Some(runtime::ResolvedPermissionMode::ReadOnly)
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        fs::write(&path, "{broken").unwrap();
        assert!(read_object(&path).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "{broken");
    }

    #[test]
    fn permission_rules_add_remove_idempotently_without_changing_other_scopes() {
        let mut root = json!({"permissions":{"deny":["Bash(rm *)"]},"model":"example"});
        permission_edit(&mut root, "add allow Bash(git status)").unwrap();
        permission_edit(&mut root, "add allow Bash(git status)").unwrap();
        assert_eq!(root["permissions"]["allow"], json!(["Bash(git status)"]));
        permission_edit(&mut root, "remove allow Bash(git status)").unwrap();
        assert_eq!(root["permissions"]["allow"], json!([]));
        assert_eq!(root["permissions"]["deny"], json!(["Bash(rm *)"]));
        assert_eq!(root["model"], "example");
        assert!(permission_edit(&mut root, "remove deny Bash(inherited)").is_err());
        assert!(permission_edit(&mut root, "add invalid Bash(ls)").is_err());
    }

    #[test]
    fn permission_rules_render_one_rule_per_line_and_mark_empty_lists() {
        let temp = Temp::new();
        write_object(
            &temp.0.join(".claw/settings.local.json"),
            &json!({"permissions":{"allow":["Bash(git status)","Read"],"deny":["Bash(rm *)"]}}),
        )
        .unwrap();
        let config = runtime::ConfigLoader::new(&temp.0, temp.0.join("config"))
            .load()
            .unwrap();
        let text = render_permission_rules(config.permission_rules());
        assert!(text.contains("  allow\n    Bash(git status)\n    Read\n"));
        assert!(text.contains("  ask\n    (none)\n"));
        assert!(text.contains("  deny\n    Bash(rm *)\n"));
        assert!(!text.contains("RuntimePermissionRuleConfig"));
    }

    #[test]
    fn mcp_disable_can_be_overridden_by_enable_at_a_more_specific_scope() {
        let temp = Temp::new();
        let home = temp.0.join("config");
        write_object(
            &home.join("settings.json"),
            &json!({"mcpServerStates":{"example":false,"other":false}}),
        )
        .unwrap();
        write_object(
            &temp.0.join(".claw/settings.local.json"),
            &json!({"mcpServerStates":{"example":true}}),
        )
        .unwrap();
        let config = runtime::ConfigLoader::new(&temp.0, home).load().unwrap();
        assert_eq!(
            disabled_servers(&config),
            std::collections::BTreeSet::from(["other".to_string()])
        );
    }

    #[test]
    fn artifact_paths_reject_traversal_and_symlink_escape() {
        let temp = Temp::new();
        let root = temp.0.join("artifacts");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("diagram.svg"), "<svg/>").unwrap();
        fs::write(temp.0.join("outside"), "private").unwrap();
        assert!(contained_artifact(&root, "diagram.svg").is_ok());
        assert!(contained_artifact(&root, "../outside").is_err());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(temp.0.join("outside"), root.join("escape")).unwrap();
            assert!(contained_artifact(&root, "escape").is_err());
        }
    }

    #[test]
    fn terminal_binding_install_preserves_entries_backs_up_and_is_idempotent() {
        let temp = Temp::new();
        let path = temp.0.join("keybindings.json");
        let before = r#"[{"key":"ctrl+k","command":"existing"}]"#;
        fs::write(&path, before).unwrap();
        install_binding(&path).unwrap();
        install_binding(&path).unwrap();
        let bindings: Vec<Value> =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(bindings.len(), 2);
        assert_eq!(bindings[0]["command"], "existing");
        assert_eq!(bindings[1]["args"]["text"], "\u{1b}\r");
        assert_eq!(
            fs::read_to_string(path.with_extension("json.claw-backup")).unwrap(),
            before
        );
        fs::write(&path, "[// JSONC comment\n]").unwrap();
        assert!(install_binding(&path).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "[// JSONC comment\n]");
    }

    #[test]
    fn terminal_binding_conflict_does_not_overwrite_user_choice() {
        let temp = Temp::new();
        let path = temp.0.join("keybindings.json");
        let before = r#"[{"key":"shift+enter","command":"other","when":"terminalFocus"}]"#;
        fs::write(&path, before).unwrap();
        assert!(install_binding(&path).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), before);
    }

    #[test]
    fn reference_commands_require_briefs_and_disclose_local_artifacts() {
        for name in ["/design", "/slides"] {
            assert!(reference_prompt(name, "").is_err());
            let prompt = reference_prompt(name, "Quarterly review").unwrap();
            assert!(prompt.contains("Quarterly review"));
            assert!(prompt.contains(".claw/artifacts"));
        }
        assert!(reference_prompt("/missing", "x").is_err());
        assert_eq!(
            account_url("/usage-credits"),
            Some("https://claude.ai/settings/usage")
        );
        assert_eq!(account_url("/missing"), None);
    }
}
