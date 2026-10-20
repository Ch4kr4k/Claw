//! Learning between sessions (after hermes' background review and curator).
//!
//! After a turn that took several tool calls, a background request (the
//! auxiliary model when set, else a hosted main model) reads what the turn
//! did and decides whether anything durable is worth keeping: a memory (the
//! same files the memory instructions describe) or a skill, a reusable
//! procedure saved as `~/.claw/skills/<name>/SKILL.md`. It never touches
//! the conversation. While claw is idle, at most once a week, the curator
//! merges or archives the skills it learned (never deletes, never touches
//! skills a person wrote). `"learning": "off"` in settings turns both off.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use api::{InputMessage, MessageRequest, ProviderClient as ApiProviderClient};
use runtime::ConversationMessage;
use serde_json::Value;

/// A turn needs at least this many tool calls to be reviewed: short
/// exchanges rarely teach anything, and every review is a request.
const MIN_TOOL_CALLS: usize = 4;
/// Marks a skill as learned by claw, so only those are updated or curated.
const LEARNED_MARK: &str = "learned-by: claw";
const CURATE_EVERY: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// What the background review saved, shown at the next prompt.
static LEARNED: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn learning_on() -> bool {
    !runtime::read_user_setting("learning")
        .is_some_and(|value| matches!(value.trim(), "off" | "false" | "0" | "no"))
}

fn skills_dir() -> PathBuf {
    runtime::default_config_home().join("skills")
}

/// Asks the side-task model (else a hosted main model) for JSON. `None`
/// when only a local model is available (a review would hold the GPU after
/// every turn) or the request fails.
fn ask_json(main_model: &str, system: &str, user: String) -> Option<Value> {
    let main_hosted = !main_model.starts_with("local/");
    if runtime::read_user_setting("auxiliaryModel").is_none() && !main_hosted {
        return None;
    }
    let main_client = main_hosted
        .then(|| ApiProviderClient::from_model_with_anthropic_auth(main_model, None).ok())
        .flatten();
    let request = MessageRequest {
        model: main_model.to_string(),
        max_tokens: 8_192,
        messages: vec![InputMessage::user_text(user)],
        system: Some(system.to_string()),
        stream: false,
        ..Default::default()
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()?;
    let fallback = main_client.as_ref().map(|client| (main_model, client));
    let text = crate::side_task_text(&runtime, request, fallback).ok()?;
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    serde_json::from_str(text.get(start..=end)?).ok()
}

/// `My Skill!` -> `my-skill`: a file and folder name.
fn slug(name: &str) -> String {
    let mut slug = String::new();
    for ch in name.trim().chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
        } else if !slug.ends_with('-') && !slug.is_empty() {
            slug.push('-');
        }
    }
    slug.trim_end_matches('-').chars().take(60).collect()
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

const REVIEW_SYSTEM: &str = "You review one finished turn of a coding agent's work and decide \
whether anything should be kept for future sessions. Keep only what is durable and not \
recorded in the code, git history or instruction files: a user preference or correction, a \
project fact learned the hard way, a decision and its reason, or a mistake and its fix (memories); \
or a reusable multi-step procedure that worked and will be needed again, such as how to build, \
test, deploy or debug something in this setup (skills). Most turns teach nothing: then return \
empty lists. Never keep secrets, keys or passwords. Prefer updating an existing memory or skill \
(use its exact file or name) over adding a near duplicate. Reply with JSON only:\n\
{\"memories\": [{\"file\": \"kebab-slug\", \"description\": \"one line\", \
\"type\": \"user|feedback|project|reference\", \"body\": \"the fact; for feedback and project \
add a **Why:** and a **How to apply:** line\"}], \"skills\": [{\"name\": \"kebab-slug\", \
\"description\": \"one line: what it does and when to use it\", \"body\": \"markdown steps\"}]}\n\
At most 3 memories and 1 skill.";

/// Starts the background review of a finished turn (`turn` = the
/// messages it added) when it took enough tool calls. Returns at once.
pub(crate) fn review_turn_in_background(main_model: String, turn: Vec<ConversationMessage>) {
    let tool_calls = turn
        .iter()
        .flat_map(|message| &message.blocks)
        .filter(|block| matches!(block, runtime::ContentBlock::ToolUse { .. }))
        .count();
    if tool_calls < MIN_TOOL_CALLS || !learning_on() {
        return;
    }
    let Ok(cwd) = std::env::current_dir() else {
        return;
    };
    std::thread::spawn(move || review_turn(&main_model, &turn, &cwd));
}

fn review_turn(main_model: &str, turn: &[ConversationMessage], cwd: &Path) {
    let memory_dir = runtime::memory_dir(cwd);
    let index = fs::read_to_string(memory_dir.join(runtime::MEMORY_INDEX)).unwrap_or_default();
    let skills = learned_skills()
        .iter()
        .map(|(name, description, _)| format!("- {name}: {description}"))
        .collect::<Vec<_>>()
        .join("\n");
    let user =
        format!(
        "Existing memories (MEMORY.md):\n{}\n\nSkills you learned before:\n{}\n\nThe turn:\n\n{}",
        if index.trim().is_empty() { "(none)" } else { index.trim() },
        if skills.is_empty() { "(none)" } else { &skills },
        runtime::summary_transcript(None, turn)
    );
    let Some(reply) = ask_json(main_model, REVIEW_SYSTEM, user) else {
        return;
    };
    let mut saved = Vec::new();
    for memory in reply["memories"].as_array().into_iter().flatten() {
        if let Some(name) = save_memory(&memory_dir, memory) {
            saved.push(format!("memory {name}"));
        }
    }
    for skill in reply["skills"].as_array().into_iter().flatten() {
        if let Some(name) = save_skill(skill) {
            saved.push(format!("skill {name}"));
        }
    }
    if !saved.is_empty() {
        LEARNED
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(saved.join(", "));
    }
}

/// Prints what background reviews saved since the last prompt.
pub(crate) fn announce_learned() {
    let learned = std::mem::take(
        &mut *LEARNED
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    );
    for line in learned {
        println!("\x1b[2m  ⎿ learned: {line}\x1b[0m");
    }
}

/// Writes one memory file (frontmatter as the memory instructions describe)
/// and its index line; an existing file of the same name is updated.
fn save_memory(dir: &Path, memory: &Value) -> Option<String> {
    let name = slug(memory["file"].as_str()?.trim_end_matches(".md"));
    let body = memory["body"].as_str()?.trim();
    if name.is_empty() || body.is_empty() {
        return None;
    }
    let description = one_line(memory["description"].as_str().unwrap_or(&name));
    let kind = memory["type"]
        .as_str()
        .filter(|kind| matches!(*kind, "user" | "feedback" | "project" | "reference"))
        .unwrap_or("project");
    fs::create_dir_all(dir).ok()?;
    let file = format!("{name}.md");
    fs::write(
        dir.join(&file),
        format!(
            "---\nname: {name}\ndescription: {description}\nmetadata:\n  type: {kind}\n---\n\n{body}\n"
        ),
    )
    .ok()?;
    let index_path = dir.join(runtime::MEMORY_INDEX);
    let index = fs::read_to_string(&index_path).unwrap_or_default();
    if !index.contains(&format!("({file})")) {
        let mut index = index;
        if !index.is_empty() && !index.ends_with('\n') {
            index.push('\n');
        }
        index.push_str(&format!("- [{name}]({file}) — {description}\n"));
        fs::write(&index_path, index).ok()?;
    }
    Some(name)
}

/// Writes `~/.claw/skills/<name>/SKILL.md`. A skill a person wrote (one
/// without [`LEARNED_MARK`]) is never overwritten.
fn save_skill(skill: &Value) -> Option<String> {
    let name = slug(skill["name"].as_str()?);
    let body = skill["body"].as_str()?.trim();
    if name.is_empty() || body.is_empty() {
        return None;
    }
    let description = one_line(skill["description"].as_str().unwrap_or(&name));
    let dir = skills_dir().join(&name);
    let file = dir.join("SKILL.md");
    if fs::read_to_string(&file).is_ok_and(|text| !text.contains(LEARNED_MARK)) {
        return None;
    }
    fs::create_dir_all(&dir).ok()?;
    fs::write(
        &file,
        format!("---\nname: {name}\ndescription: {description}\n{LEARNED_MARK}\n---\n\n{body}\n"),
    )
    .ok()?;
    Some(name)
}

/// The skills claw learned: (name, description, path).
fn learned_skills() -> Vec<(String, String, PathBuf)> {
    let Ok(entries) = fs::read_dir(skills_dir()) else {
        return Vec::new();
    };
    let mut skills = entries
        .flatten()
        .filter_map(|entry| {
            let file = entry.path().join("SKILL.md");
            let text = fs::read_to_string(&file).ok()?;
            if !text.contains(LEARNED_MARK) {
                return None;
            }
            let field = |key: &str| {
                text.lines()
                    .find_map(|line| line.strip_prefix(key))
                    .map(|value| value.trim().to_string())
            };
            Some((
                field("name:")?,
                field("description:").unwrap_or_default(),
                file,
            ))
        })
        .collect::<Vec<_>>();
    skills.sort();
    skills
}

const CURATE_SYSTEM: &str = "You maintain a coding agent's library of skills it learned \
itself. Merge skills that cover the same procedure into one, and archive skills that are \
too specific to one past task or are superseded. Keep everything else as it is. Reply with \
JSON only: {\"archive\": [\"name\"], \"merge\": [{\"into\": \"name\", \"from\": [\"name\"], \
\"description\": \"one line\", \"body\": \"the merged markdown steps\"}]}. Empty lists when \
the library is fine.";

/// Tidies the learned skills in the background when the last run was a
/// week ago or more (hermes' curator). Archived skills move to
/// `~/.claw/skills/.archive/`, so nothing is lost.
pub(crate) fn curate_in_background(main_model: String) {
    if !learning_on() {
        return;
    }
    let state = skills_dir().join(".curator_state");
    let last = fs::read_to_string(&state)
        .ok()
        .and_then(|text| text.trim().parse::<u64>().ok())
        .unwrap_or(0);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    if now.saturating_sub(last) < CURATE_EVERY.as_secs() || learned_skills().len() < 2 {
        return;
    }
    std::thread::spawn(move || {
        let _ = fs::create_dir_all(skills_dir());
        let _ = fs::write(&state, now.to_string());
        curate(&main_model);
    });
}

fn curate(main_model: &str) {
    let skills = learned_skills();
    let library = skills
        .iter()
        .map(|(name, description, file)| {
            let body: String = fs::read_to_string(file)
                .unwrap_or_default()
                .chars()
                .take(2_000)
                .collect();
            format!("## {name}\n{description}\n\n{body}")
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    let Some(reply) = ask_json(main_model, CURATE_SYSTEM, library) else {
        return;
    };
    let known = |name: &str| skills.iter().any(|(known, _, _)| known == name);
    let archive = |name: &str| {
        let from = skills_dir().join(name);
        let to = skills_dir().join(".archive").join(name);
        let _ = fs::create_dir_all(skills_dir().join(".archive"));
        fs::rename(from, to).is_ok()
    };
    let mut changed = Vec::new();
    for merge in reply["merge"].as_array().into_iter().flatten() {
        let Some(into) = merge["into"].as_str().filter(|name| known(name)) else {
            continue;
        };
        let skill = serde_json::json!({
            "name": into,
            "description": merge["description"],
            "body": merge["body"],
        });
        if save_skill(&skill).is_none() {
            continue;
        }
        for from in merge["from"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            if from != into && known(from) && archive(from) {
                changed.push(format!("merged {from} into {into}"));
            }
        }
    }
    for name in reply["archive"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        if known(name) && archive(name) {
            changed.push(format!("archived {name}"));
        }
    }
    if !changed.is_empty() {
        LEARNED
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(format!("skills tidied: {}", changed.join(", ")));
    }
}

#[cfg(test)]
mod tests {
    use super::{learned_skills, save_memory, save_skill, slug};
    use serde_json::json;

    #[test]
    fn slugs_are_file_safe() {
        assert_eq!(slug("Build & Test lightcomp!"), "build-test-lightcomp");
        assert_eq!(slug("  ok  "), "ok");
    }

    #[test]
    fn saves_memories_with_an_index_line_and_keeps_human_skills() {
        let _guard = crate::test_lock::lock();
        let home = std::env::temp_dir().join(format!("claw-learning-{}", std::process::id()));
        let original = std::env::var("CLAW_CONFIG_HOME").ok();
        std::env::set_var("CLAW_CONFIG_HOME", &home);
        let memory_dir = home.join("memory");

        let memory = json!({"file": "zstd-choice", "description": "we use zstd",
            "type": "project", "body": "Compression is zstd.\n**Why:** speed."});
        assert_eq!(
            save_memory(&memory_dir, &memory).as_deref(),
            Some("zstd-choice")
        );
        assert_eq!(
            save_memory(&memory_dir, &memory).as_deref(),
            Some("zstd-choice")
        );
        let index = std::fs::read_to_string(memory_dir.join("MEMORY.md")).unwrap();
        assert_eq!(index.matches("(zstd-choice.md)").count(), 1, "{index}");
        let file = std::fs::read_to_string(memory_dir.join("zstd-choice.md")).unwrap();
        assert!(file.starts_with("---\nname: zstd-choice\n"));

        let skill = json!({"name": "build lightcomp", "description": "how to build",
            "body": "1. make GL=1"});
        assert_eq!(save_skill(&skill).as_deref(), Some("build-lightcomp"));
        assert_eq!(learned_skills().len(), 1);
        // A skill a person wrote is left alone.
        let human = home.join("skills").join("deploy");
        std::fs::create_dir_all(&human).unwrap();
        std::fs::write(human.join("SKILL.md"), "---\nname: deploy\n---\nmine").unwrap();
        assert!(save_skill(&json!({"name": "deploy", "body": "x"})).is_none());
        assert_eq!(
            std::fs::read_to_string(human.join("SKILL.md")).unwrap(),
            "---\nname: deploy\n---\nmine"
        );

        let _ = std::fs::remove_dir_all(&home);
        match original {
            Some(value) => std::env::set_var("CLAW_CONFIG_HOME", value),
            None => std::env::remove_var("CLAW_CONFIG_HOME"),
        }
    }
}
