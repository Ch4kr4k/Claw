//! The learning loop: after a turn that worked something out, claw reviews
//! it in the background and saves what will help next time, as **skills**
//! (procedures: how to do a kind of task here, with its pitfalls and how to
//! check it) and **memories** (facts: who the user is, how they want claw to
//! work, project decisions). Based on hermes-agent's background review,
//! skill manager and curator, with these changes:
//!
//! - **Only turns that taught something are reviewed**: many tool calls, a
//!   failure that was then overcome, a correction from the user, a learned
//!   skill that was used and went wrong ([`should_review`]). A plain answer
//!   costs nothing.
//! - **The review proposes, claw applies**: the model answers with a JSON
//!   list of changes and claw writes them itself ([`apply_review`]), so the
//!   review needs no tools and works with small local models. Every change
//!   is checked: names, sizes, prompt-injection and persistence patterns
//!   ([`crate::threats`], strict scope), and only skills claw learned can be
//!   changed (never ones the user wrote).
//! - **Everything is undoable**: each change is journaled with what it
//!   replaced, and `/learn undo` reverts the last review.
//! - **Skills improve with use**: loading a skill is counted; a learned
//!   skill used in a turn that had failures is shown to the review with its
//!   text, so it gets fixed where it was wrong.
//! - **The curator keeps the set small**: learned skills unused for many
//!   sessions are marked stale in the list the model sees, then archived
//!   (moved aside, recoverable); look-alike skills are offered to the
//!   review to merge. Time away does not age them, only sessions do.
//!
//! Learned skills live in `~/.claw/skills/<name>/SKILL.md` (where claw's
//! Skill tool already looks), marked `source: learned`; the ledger and the
//! journal in `~/.claw/learning/`. Memories go to the project's memory
//! folder ([`crate::memory`]).
//!
//! On by default; setting `"learningLoop": false` or `CLAW_LEARNING=0`
//! turns it off.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::config::default_config_home;
use crate::session::{ContentBlock, ConversationMessage, MessageRole};

/// Sessions without use after which a learned skill is shown as stale.
const STALE_AFTER_SESSIONS: u64 = 12;
/// ...and after which it is archived (unless pinned).
const ARCHIVE_AFTER_SESSIONS: u64 = 40;
const MAX_SKILL_BODY: usize = 12_000;
const MIN_SKILL_BODY: usize = 80;
const MAX_MEMORY_BODY: usize = 4_000;
const MAX_DESCRIPTION: usize = 300;
/// Skills listed in the system prompt.
const MAX_LISTED_SKILLS: usize = 60;

// ------------------------------------------------------------------ settings

/// Whether the learning loop is on.
#[must_use]
pub fn learning_enabled() -> bool {
    if let Ok(value) = std::env::var("CLAW_LEARNING") {
        return !matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off" | "no"
        );
    }
    // `"learning": "off"` is the setting of claw's earlier learning loop.
    if crate::config::read_user_setting("learning")
        .is_some_and(|value| matches!(value.trim(), "off" | "false" | "0" | "no"))
    {
        return false;
    }
    crate::config::read_user_setting_value("learningLoop")
        .and_then(|value| value.as_bool())
        .unwrap_or(true)
}

/// Where learned skills are written.
#[must_use]
pub fn learned_skills_dir() -> PathBuf {
    default_config_home().join("skills")
}

fn learning_dir() -> PathBuf {
    default_config_home().join("learning")
}

// -------------------------------------------------------------------- ledger

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct SkillStats {
    #[serde(default)]
    uses: u32,
    /// Session count when it was created / last used.
    #[serde(default)]
    created_session: u64,
    #[serde(default)]
    last_used_session: u64,
    #[serde(default)]
    pinned: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Ledger {
    #[serde(default)]
    sessions: u64,
    #[serde(default)]
    skills: BTreeMap<String, SkillStats>,
}

fn load_ledger(dir: &Path) -> Ledger {
    fs::read_to_string(dir.join("ledger.json"))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn save_ledger(dir: &Path, ledger: &Ledger) {
    if fs::create_dir_all(dir).is_ok() {
        if let Ok(text) = serde_json::to_string_pretty(ledger) {
            let _ = fs::write(dir.join("ledger.json"), text);
        }
    }
}

/// Counts a session and runs the curator. Called once at startup.
pub fn record_session_start() {
    if !learning_enabled() {
        return;
    }
    let store = Store::default_store();
    for notice in store.start_session() {
        push_notice(notice);
    }
}

/// Counts a use of skill `name` (the Skill tool loaded it).
pub fn record_skill_use(name: &str) {
    Store::default_store().record_use(name);
}

/// Pins or unpins a learned skill (pinned skills are never archived).
pub fn set_pinned(name: &str, pinned: bool) -> Result<String, String> {
    Store::default_store().set_pinned(name, pinned)
}

// --------------------------------------------------------------------- store

/// The folders the learning loop works in (separate for tests).
#[derive(Debug, Clone)]
pub struct Store {
    pub skills: PathBuf,
    pub state: PathBuf,
}

/// A skill found on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillInfo {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
    pub learned: bool,
}

impl Store {
    #[must_use]
    pub fn default_store() -> Self {
        Self {
            skills: learned_skills_dir(),
            state: learning_dir(),
        }
    }

    fn start_session(&self) -> Vec<String> {
        let mut ledger = load_ledger(&self.state);
        ledger.sessions += 1;
        let session = ledger.sessions;
        let learned = list_skills_in(&self.skills)
            .into_iter()
            .filter(|skill| skill.learned)
            .collect::<Vec<_>>();
        let mut notices = Vec::new();
        let mut archived = Vec::new();
        for skill in &learned {
            let stats = ledger
                .skills
                .entry(skill.name.clone())
                .or_insert_with(|| SkillStats {
                    created_session: session,
                    ..SkillStats::default()
                });
            let idle = session.saturating_sub(stats.last_used_session.max(stats.created_session));
            if idle >= ARCHIVE_AFTER_SESSIONS && !stats.pinned {
                archived.push(skill.clone());
            }
        }
        if !archived.is_empty() {
            let batch = new_batch_id("curator");
            for skill in &archived {
                if let Ok(entry) = self.archive_skill(&skill.name, &batch) {
                    ledger.skills.remove(&skill.name);
                    let _ = append_journal(&self.state, &entry);
                    notices.push(format!(
                        "archived learned skill `{}` (unused for {ARCHIVE_AFTER_SESSIONS} sessions)",
                        skill.name
                    ));
                }
            }
        }
        // Forget stats of skills that are gone.
        let names = learned
            .iter()
            .map(|skill| skill.name.clone())
            .collect::<Vec<_>>();
        ledger.skills.retain(|name, _| names.contains(name));
        save_ledger(&self.state, &ledger);
        if notices.is_empty() {
            notices
        } else {
            vec![format!(
                "{} — /learn undo restores them",
                notices.join("; ")
            )]
        }
    }

    fn record_use(&self, name: &str) {
        let name = name.trim().trim_start_matches('/');
        if !self.skills.join(name).join("SKILL.md").is_file() {
            return;
        }
        let mut ledger = load_ledger(&self.state);
        let session = ledger.sessions;
        let stats = ledger.skills.entry(name.to_string()).or_default();
        stats.uses += 1;
        stats.last_used_session = session;
        save_ledger(&self.state, &ledger);
        USED_THIS_TURN
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(name.to_string());
    }

    fn set_pinned(&self, name: &str, pinned: bool) -> Result<String, String> {
        let skill = self
            .learned_skill(name)
            .ok_or_else(|| format!("no learned skill named `{name}`"))?;
        let mut ledger = load_ledger(&self.state);
        ledger.skills.entry(skill.name.clone()).or_default().pinned = pinned;
        save_ledger(&self.state, &ledger);
        Ok(if pinned {
            format!("pinned `{}`: it is never archived", skill.name)
        } else {
            format!("unpinned `{}`", skill.name)
        })
    }

    fn learned_skill(&self, name: &str) -> Option<SkillInfo> {
        list_skills_in(&self.skills)
            .into_iter()
            .find(|skill| skill.learned && skill.name == name)
    }

    fn archive_skill(&self, name: &str, batch: &str) -> Result<JournalEntry, String> {
        let dir = self.skills.join(name);
        let file = dir.join("SKILL.md");
        let before = fs::read_to_string(&file).map_err(|error| error.to_string())?;
        let archive = self
            .skills
            .join(".archive")
            .join(format!("{name}-{}", today()));
        fs::create_dir_all(archive.parent().unwrap_or(&self.skills))
            .map_err(|error| error.to_string())?;
        fs::rename(&dir, &archive).map_err(|error| error.to_string())?;
        Ok(JournalEntry {
            batch: batch.to_string(),
            time: now(),
            kind: "skill".to_string(),
            action: "archive".to_string(),
            name: name.to_string(),
            path: file,
            before: Some(before),
            archived_to: Some(archive),
            index_line: None,
            index_path: None,
        })
    }
}

/// The skills in `dir` (`<dir>/<name>/SKILL.md`).
fn list_skills_in(dir: &Path) -> Vec<SkillInfo> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut skills = entries
        .flatten()
        .filter(|entry| !entry.file_name().to_string_lossy().starts_with('.'))
        .filter_map(|entry| {
            let path = entry.path().join("SKILL.md");
            let text = fs::read_to_string(&path).ok()?;
            let folder = entry.file_name().to_string_lossy().into_owned();
            Some(SkillInfo {
                name: field(&text, "name").unwrap_or(folder),
                description: field(&text, "description").unwrap_or_default(),
                learned: is_learned(&text),
                path,
            })
        })
        .collect::<Vec<_>>();
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    skills
}

/// Whether a skill file was written by claw (`source: learned`, or the
/// `learned-by: claw` mark of claw's earlier learning loop).
fn is_learned(text: &str) -> bool {
    field(text, "source").as_deref() == Some("learned")
        || field(text, "learned-by").as_deref() == Some("claw")
}

/// A `key: value` line of a file's frontmatter.
fn field(text: &str, key: &str) -> Option<String> {
    let mut lines = text.lines();
    if lines.next()?.trim() != "---" {
        return None;
    }
    lines
        .take_while(|line| line.trim() != "---")
        .find_map(|line| {
            let (found, value) = line.trim().split_once(':')?;
            (found.trim() == key)
                .then(|| value.trim().trim_matches('"').to_string())
                .filter(|value| !value.is_empty())
        })
}

/// The body of a file with frontmatter (what follows it).
fn body_of(text: &str) -> &str {
    if let Some(rest) = text.strip_prefix("---") {
        if let Some(end) = rest.find("\n---") {
            return rest[end + 4..].trim_start_matches(['\n', '\r']);
        }
    }
    text
}

/// Every skill the model can load, from the folders claw's Skill tool
/// reads: the project's (`.claw/skills`, `.claude/skills`), claw's (where
/// learned ones go) and Claude Code's. A name is listed once, from the
/// first folder that has it.
#[must_use]
pub fn available_skills(cwd: &Path) -> Vec<SkillInfo> {
    let mut dirs = Vec::new();
    for ancestor in cwd.ancestors() {
        dirs.push(ancestor.join(".claw").join("skills"));
        dirs.push(ancestor.join(".claude").join("skills"));
    }
    dirs.push(learned_skills_dir());
    if let Some(claude) = crate::config::claude_config_home() {
        dirs.push(claude.join("skills"));
    }
    let mut seen = Vec::new();
    let mut skills = Vec::new();
    for dir in dirs {
        for skill in list_skills_in(&dir) {
            if !seen.contains(&skill.name) {
                seen.push(skill.name.clone());
                skills.push(skill);
            }
        }
    }
    skills
}

/// The system-prompt section listing the skills, or `None` without any.
#[must_use]
pub fn skills_section(cwd: &Path) -> Option<String> {
    let skills = available_skills(cwd);
    if skills.is_empty() {
        return None;
    }
    let ledger = load_ledger(&learning_dir());
    let mut lines = vec![
        "# Skills".to_string(),
        "Skills are saved procedures for recurring tasks. When a task matches one, load it with the \
         Skill tool (skill: <name>) before starting, and follow it. Skills marked (learned) were \
         written by claw from earlier sessions; if one turns out wrong or incomplete, say so in \
         your reply, and it is corrected after the turn."
            .to_string(),
    ];
    for skill in skills.iter().take(MAX_LISTED_SKILLS) {
        let mut description = skill.description.clone();
        if description.chars().count() > 160 {
            description = description.chars().take(157).collect::<String>() + "…";
        }
        let mut tags = Vec::new();
        if skill.learned {
            tags.push("learned".to_string());
            if let Some(stats) = ledger.skills.get(&skill.name) {
                let idle = ledger
                    .sessions
                    .saturating_sub(stats.last_used_session.max(stats.created_session));
                if idle >= STALE_AFTER_SESSIONS && !stats.pinned {
                    tags.push(format!("unused for {idle} sessions"));
                }
            }
        }
        let tags = if tags.is_empty() {
            String::new()
        } else {
            format!(" ({})", tags.join(", "))
        };
        lines.push(format!("- {}{tags}: {description}", skill.name));
    }
    if skills.len() > MAX_LISTED_SKILLS {
        lines.push(format!(
            "- … and {} more (the Skill tool loads them by name)",
            skills.len() - MAX_LISTED_SKILLS
        ));
    }
    Some(lines.join("\n"))
}

// ------------------------------------------------------------- turn signals

static USED_THIS_TURN: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Learned skills loaded since the last call (this turn).
#[must_use]
pub fn take_skills_used() -> Vec<String> {
    let mut used = std::mem::take(
        &mut *USED_THIS_TURN
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    );
    used.dedup();
    used
}

/// What a finished turn did, to decide whether it is worth reviewing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TurnSignals {
    pub tool_calls: usize,
    pub failures: usize,
    /// A tool failed and later succeeded: something was worked out.
    pub recovered: bool,
    /// The user corrected claw or stated how to work.
    pub correction: bool,
    /// Learned skills loaded during the turn.
    pub skills_used: Vec<String>,
}

const CORRECTION_MARKERS: [&str; 16] = [
    "don't ",
    "dont ",
    "do not ",
    "instead",
    "wrong",
    "that's not",
    "thats not",
    "not what i",
    "always ",
    "never ",
    "from now on",
    "i prefer",
    "i want you to",
    "remember",
    "stop doing",
    "next time",
];

impl TurnSignals {
    /// The signals of the turn made of `messages` (the turn's own messages,
    /// starting with the user's request).
    #[must_use]
    pub fn from_messages(messages: &[ConversationMessage], skills_used: Vec<String>) -> Self {
        let mut signals = Self {
            skills_used,
            ..Self::default()
        };
        let mut failed_tools: Vec<String> = Vec::new();
        for message in messages {
            for block in &message.blocks {
                match block {
                    ContentBlock::ToolUse { .. } => signals.tool_calls += 1,
                    ContentBlock::ToolResult {
                        tool_name,
                        is_error,
                        ..
                    } => {
                        if *is_error {
                            signals.failures += 1;
                            failed_tools.push(tool_name.clone());
                        } else if failed_tools.contains(tool_name) {
                            signals.recovered = true;
                        }
                    }
                    ContentBlock::Text { text }
                        if message.role == MessageRole::User
                            && !text.starts_with("<system-reminder>")
                            && is_correction(text) =>
                    {
                        signals.correction = true;
                    }
                    _ => {}
                }
            }
        }
        signals
    }

    fn describe(&self) -> String {
        let mut parts = vec![format!("{} tool calls", self.tool_calls)];
        if self.failures > 0 {
            parts.push(format!("{} failed", self.failures));
        }
        if self.recovered {
            parts.push("a failure was then worked around".to_string());
        }
        if self.correction {
            parts.push("the user corrected you or said how to work".to_string());
        }
        parts.join(", ")
    }
}

fn is_correction(text: &str) -> bool {
    let lower = text.to_lowercase();
    lower.starts_with("no ")
        || lower.starts_with("no,")
        || CORRECTION_MARKERS
            .iter()
            .any(|marker| lower.contains(marker))
}

/// Whether a turn with `signals` is worth a review: it did enough work,
/// overcame a failure, was corrected, or used a learned skill that failed.
#[must_use]
pub fn should_review(signals: &TurnSignals) -> bool {
    signals.correction
        || (signals.recovered && signals.tool_calls >= 3)
        || signals.tool_calls >= 10
        || (!signals.skills_used.is_empty() && signals.failures > 0)
}

// ------------------------------------------------------------------- review

/// The instruction sent (as the last user message, after the whole
/// conversation) to the review model.
#[must_use]
pub fn review_prompt(cwd: &Path, signals: &TurnSignals) -> String {
    let store = Store::default_store();
    review_prompt_with(&store, &crate::memory::memory_dir(cwd), cwd, signals)
}

fn review_prompt_with(
    store: &Store,
    memory_dir: &Path,
    cwd: &Path,
    signals: &TurnSignals,
) -> String {
    let learned = list_skills_in(&store.skills)
        .into_iter()
        .filter(|skill| skill.learned)
        .collect::<Vec<_>>();
    let others = available_skills(cwd)
        .into_iter()
        .filter(|skill| !skill.learned)
        .map(|skill| skill.name)
        .collect::<Vec<_>>();
    let mut text = String::from(REVIEW_INSTRUCTIONS);
    text.push_str(&format!("\n\n## This turn\n{}.", signals.describe()));
    text.push_str("\n\n## Learned skills (you may update, append to or archive these)\n");
    if learned.is_empty() {
        text.push_str("(none yet)");
    }
    for skill in &learned {
        text.push_str(&format!("- {}: {}\n", skill.name, skill.description));
    }
    if !others.is_empty() {
        text.push_str(&format!(
            "\n\nOther skills (written by the user; never create a skill with these names): {}",
            others.join(", ")
        ));
    }
    for (a, b) in look_alike_pairs(&learned) {
        text.push_str(&format!(
            "\n\n`{a}` and `{b}` look alike: if they cover the same procedure, merge them (update \
             one with the combined text, archive the other)."
        ));
    }
    for name in &signals.skills_used {
        if let Some(skill) = learned.iter().find(|skill| &skill.name == name) {
            let body = fs::read_to_string(&skill.path).unwrap_or_default();
            text.push_str(&format!(
                "\n\n## Learned skill `{name}`, used this turn\nIf the turn showed it is wrong or \
                 incomplete, update it (the whole new text):\n```markdown\n{}\n```",
                cut(body_of(&body), 6_000)
            ));
        }
    }
    let index =
        fs::read_to_string(memory_dir.join(crate::memory::MEMORY_INDEX)).unwrap_or_default();
    text.push_str("\n\n## Memories already saved (MEMORY.md)\n");
    if index.trim().is_empty() {
        text.push_str("(none yet)");
    } else {
        text.push_str(&cut(index.trim(), 4_000));
    }
    text.push_str("\n\nReply with the JSON object only.\n</system-reminder>");
    text
}

const REVIEW_INSTRUCTIONS: &str = "<system-reminder>
Learning review. This message is from claw, not the user, and your reply is not shown to them. The \
turn above is finished; do not continue it, answer it or call tools. Decide whether anything from \
it should be saved for future sessions, and reply with only a JSON object:

{\"skills\": [...], \"memories\": [...]}

Skills are procedures: how to do a kind of task in this environment that took discovery to get \
right (the commands that work here, the steps and their order, the pitfalls hit and how to avoid \
them, how to check the result). Save one when this turn worked out a reusable way to do something \
non-obvious (after trial and error, a failure overcome, or a correction from the user) that is \
likely to come up again. Not for one-off facts, the moving state of today's task, or what any \
competent engineer already knows.
Memories are facts: who the user is (role, expertise), how they want you to work (corrections and \
approaches they confirmed, with why), project goals, decisions and constraints that the code and \
git history do not show, and where to find things.

Bar for saving (most turns save nothing; {\"skills\": [], \"memories\": []} is the usual answer):
- Would it still be true and useful a month from now, in a session about something else? Today's \
bug or errand fails; the stable residue (a constraint, a decision, the procedure) can pass.
- Already saved means already remembered: do not restate an existing memory or skill in other \
words. New material is a fact the store lacks, a correction, or something that supersedes an entry; \
update that entry instead of adding a near-duplicate.
- Calibrate to the evidence. What the user said is theirs; what you proposed and they only did not \
object to is not. One passing mention is not a general preference: keep a preference to the scope \
the user gave it, and never widen it.
- Do not save what you fetched or generated (search results, file contents, your own suggestions): \
it can be looked up or derived again. Do save what the user confirmed or decided about it.
- Prefer durable phrasing over details that go stale. Never save secrets, credentials or tokens.

Formats:
- New skill: {\"action\": \"create\", \"name\": \"kebab-case-name\", \"description\": \"one line: \
what it does and when to use it\", \"body\": \"markdown with the sections ## When to use, ## Steps, \
## Pitfalls, ## Check\"}
- Rewrite a learned skill: {\"action\": \"update\", \"name\": \"...\", \"body\": \"the whole new \
markdown\", \"description\": \"optional new one\"} (only for a skill whose text is shown below)
- Add to a learned skill: {\"action\": \"append\", \"name\": \"...\", \"body\": \"markdown to add at \
the end, e.g. a new pitfall\"}
- Archive a learned skill that is wrong or merged away: {\"action\": \"archive\", \"name\": \"...\"}
- Memory (created, or replaced when the name exists): {\"name\": \"kebab-case-name\", \"type\": \
\"user|feedback|project|reference\", \"description\": \"one line\", \"body\": \"the fact; for \
feedback and project add a **Why:** line and a **How to apply:** line\"}
- Delete a memory this turn proved wrong: {\"action\": \"delete\", \"name\": \"...\"}";

/// Changes proposed by a review.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct Review {
    #[serde(default)]
    pub skills: Vec<SkillChange>,
    #[serde(default)]
    pub memories: Vec<MemoryChange>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SkillChange {
    #[serde(default = "default_create")]
    pub action: String,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub body: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct MemoryChange {
    #[serde(default)]
    pub action: Option<String>,
    pub name: String,
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub body: Option<String>,
}

fn default_create() -> String {
    "create".to_string()
}

/// The review in a model's reply: the outermost JSON object in it (a code
/// fence or a sentence around it is ignored).
pub fn parse_review(reply: &str) -> Result<Review, String> {
    let start = reply
        .find('{')
        .ok_or("the review reply has no JSON object")?;
    let end = reply
        .rfind('}')
        .ok_or("the review reply has no JSON object")?;
    if end < start {
        return Err("the review reply has no JSON object".to_string());
    }
    serde_json::from_str(&reply[start..=end]).map_err(|error| format!("bad review JSON: {error}"))
}

/// One applied (or refused) change, for the user's notice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applied {
    pub summary: String,
    pub ok: bool,
}

/// Applies `review` for the project at `cwd`; journals every change as one
/// batch (one `/learn undo`).
pub fn apply_review(cwd: &Path, review: &Review) -> Vec<Applied> {
    let store = Store::default_store();
    apply_review_with(&store, &crate::memory::memory_dir(cwd), cwd, review)
}

fn apply_review_with(
    store: &Store,
    memory_dir: &Path,
    cwd: &Path,
    review: &Review,
) -> Vec<Applied> {
    let batch = new_batch_id("review");
    let mut applied = Vec::new();
    let user_skills = available_skills(cwd)
        .into_iter()
        .filter(|skill| !skill.learned)
        .map(|skill| skill.name)
        .collect::<Vec<_>>();
    for change in &review.skills {
        let result = apply_skill_change(store, &user_skills, change, &batch);
        applied.push(match result {
            Ok(summary) => Applied { summary, ok: true },
            Err(reason) => Applied {
                summary: format!("skill `{}` not saved: {reason}", change.name),
                ok: false,
            },
        });
    }
    for change in &review.memories {
        let result = apply_memory_change(store, memory_dir, change, &batch);
        applied.push(match result {
            Ok(summary) => Applied { summary, ok: true },
            Err(reason) => Applied {
                summary: format!("memory `{}` not saved: {reason}", change.name),
                ok: false,
            },
        });
    }
    applied
}

fn apply_skill_change(
    store: &Store,
    user_skills: &[String],
    change: &SkillChange,
    batch: &str,
) -> Result<String, String> {
    let name = valid_name(&change.name)?;
    if user_skills.contains(&name) {
        return Err("a skill the user wrote has that name".to_string());
    }
    let file = store.skills.join(&name).join("SKILL.md");
    let existing = fs::read_to_string(&file).ok();
    if let Some(text) = &existing {
        if !is_learned(text) {
            return Err("a skill the user wrote has that name".to_string());
        }
    }
    let action = change.action.trim().to_ascii_lowercase();
    if action == "archive" {
        if existing.is_none() {
            return Err("there is no learned skill by that name".to_string());
        }
        let entry = store.archive_skill(&name, batch)?;
        append_journal(&store.state, &entry)?;
        return Ok(format!("archived skill `{name}`"));
    }
    let body = change.body.as_deref().unwrap_or_default().trim();
    let (text, verb) = match (action.as_str(), &existing) {
        ("append", Some(old)) => {
            if body.is_empty() {
                return Err("nothing to add".to_string());
            }
            if body_of(old).contains(body) {
                return Err("the skill already says that".to_string());
            }
            let description = field(old, "description").unwrap_or_default();
            let created = field(old, "created").unwrap_or_else(today);
            let new_body = format!("{}\n\n{body}", body_of(old).trim_end());
            (
                skill_file(&name, &description, &created, &new_body),
                "added to",
            )
        }
        ("append", None) => return Err("there is no learned skill by that name".to_string()),
        (_, existing) => {
            let description = change
                .description
                .clone()
                .or_else(|| {
                    existing
                        .as_deref()
                        .and_then(|old| field(old, "description"))
                })
                .unwrap_or_default();
            let description = valid_description(&description)?;
            if body.len() < MIN_SKILL_BODY {
                return Err("its text is too short to be a procedure".to_string());
            }
            let created = existing
                .as_deref()
                .and_then(|old| field(old, "created"))
                .unwrap_or_else(today);
            let verb = if existing.is_some() { "updated" } else { "new" };
            (skill_file(&name, &description, &created, body), verb)
        }
    };
    if body_of(&text).len() > MAX_SKILL_BODY {
        return Err(format!("its text is over {MAX_SKILL_BODY} characters"));
    }
    let found = crate::threats::scan(&text, crate::threats::Scope::Strict);
    if !found.is_empty() {
        return Err(format!(
            "it matched security patterns ({})",
            found.join(", ")
        ));
    }
    if existing.as_deref() == Some(text.as_str()) {
        return Err("no change".to_string());
    }
    fs::create_dir_all(file.parent().unwrap_or(&store.skills))
        .map_err(|error| error.to_string())?;
    fs::write(&file, &text).map_err(|error| error.to_string())?;
    append_journal(
        &store.state,
        &JournalEntry {
            batch: batch.to_string(),
            time: now(),
            kind: "skill".to_string(),
            action: verb.to_string(),
            name: name.clone(),
            path: file,
            before: existing,
            archived_to: None,
            index_line: None,
            index_path: None,
        },
    )?;
    if verb == "new" {
        let mut ledger = load_ledger(&store.state);
        let session = ledger.sessions;
        ledger.skills.insert(
            name.clone(),
            SkillStats {
                created_session: session,
                ..SkillStats::default()
            },
        );
        save_ledger(&store.state, &ledger);
    }
    Ok(format!("{verb} skill `{name}`"))
}

fn skill_file(name: &str, description: &str, created: &str, body: &str) -> String {
    format!(
        "---\nname: {name}\ndescription: {}\nsource: learned\ncreated: {created}\nupdated: {}\n---\n\n{}\n",
        description.replace('\n', " "),
        today(),
        body.trim()
    )
}

fn apply_memory_change(
    store: &Store,
    memory_dir: &Path,
    change: &MemoryChange,
    batch: &str,
) -> Result<String, String> {
    let name = valid_name(&change.name)?;
    let file = memory_dir.join(format!("{name}.md"));
    let index_path = memory_dir.join(crate::memory::MEMORY_INDEX);
    let existing = fs::read_to_string(&file).ok();
    let action = change
        .action
        .as_deref()
        .unwrap_or("save")
        .trim()
        .to_ascii_lowercase();
    if action == "delete" {
        let before = existing.ok_or("there is no memory by that name")?;
        fs::remove_file(&file).map_err(|error| error.to_string())?;
        let removed = remove_index_line(&index_path, &format!("({name}.md)"));
        append_journal(
            &store.state,
            &JournalEntry {
                batch: batch.to_string(),
                time: now(),
                kind: "memory".to_string(),
                action: "deleted".to_string(),
                name: name.clone(),
                path: file,
                before: Some(before),
                archived_to: None,
                index_line: removed,
                index_path: Some(index_path),
            },
        )?;
        return Ok(format!("deleted memory `{name}`"));
    }
    let kind = change
        .kind
        .as_deref()
        .unwrap_or("project")
        .trim()
        .to_ascii_lowercase();
    if !matches!(kind.as_str(), "user" | "feedback" | "project" | "reference") {
        return Err(format!("unknown memory type `{kind}`"));
    }
    let description = valid_description(change.description.as_deref().unwrap_or_default())?;
    let body = change.body.as_deref().unwrap_or_default().trim();
    if body.is_empty() || body.len() > MAX_MEMORY_BODY {
        return Err(format!(
            "its text must be 1 to {MAX_MEMORY_BODY} characters"
        ));
    }
    let text = format!(
        "---\nname: {name}\ndescription: {description}\nmetadata:\n  type: {kind}\n  source: learned\n---\n\n{body}\n"
    );
    let found = crate::threats::scan(&text, crate::threats::Scope::Strict);
    if !found.is_empty() {
        return Err(format!(
            "it matched security patterns ({})",
            found.join(", ")
        ));
    }
    if existing.as_deref() == Some(text.as_str()) {
        return Err("no change".to_string());
    }
    fs::create_dir_all(memory_dir).map_err(|error| error.to_string())?;
    fs::write(&file, &text).map_err(|error| error.to_string())?;
    let line = format!("- [{}]({name}.md) — {description}", title_of(&name));
    let index = fs::read_to_string(&index_path).unwrap_or_default();
    let added = if index.contains(&format!("({name}.md)")) {
        None
    } else {
        let mut index = index;
        if !index.is_empty() && !index.ends_with('\n') {
            index.push('\n');
        }
        index.push_str(&line);
        index.push('\n');
        fs::write(&index_path, index).map_err(|error| error.to_string())?;
        Some(line)
    };
    let verb = if existing.is_some() { "updated" } else { "new" };
    append_journal(
        &store.state,
        &JournalEntry {
            batch: batch.to_string(),
            time: now(),
            kind: "memory".to_string(),
            action: verb.to_string(),
            name: name.clone(),
            path: file,
            before: existing,
            archived_to: None,
            index_line: added,
            index_path: Some(index_path),
        },
    )?;
    Ok(format!("{verb} memory `{name}`"))
}

fn title_of(name: &str) -> String {
    let mut title = name.replace('-', " ");
    if let Some(first) = title.get(..1) {
        title = first.to_uppercase() + &title[1..];
    }
    title
}

fn remove_index_line(index_path: &Path, needle: &str) -> Option<String> {
    let index = fs::read_to_string(index_path).ok()?;
    let line = index
        .lines()
        .find(|line| line.contains(needle))?
        .to_string();
    let rest = index
        .lines()
        .filter(|other| *other != line)
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(index_path, rest + "\n").ok()?;
    Some(line)
}

fn valid_name(name: &str) -> Result<String, String> {
    let name = name.trim().to_ascii_lowercase();
    let ok = (3..=64).contains(&name.len())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !name.starts_with('-')
        && !name.ends_with('-');
    if ok {
        Ok(name)
    } else {
        Err("the name must be 3-64 characters of a-z, 0-9 and -".to_string())
    }
}

fn valid_description(description: &str) -> Result<String, String> {
    let description = description.trim().replace('\n', " ");
    if description.is_empty() || description.len() > MAX_DESCRIPTION {
        return Err(format!(
            "its description must be 1 to {MAX_DESCRIPTION} characters"
        ));
    }
    Ok(description)
}

/// Pairs of learned skills whose names and descriptions share most words.
fn look_alike_pairs(skills: &[SkillInfo]) -> Vec<(String, String)> {
    let words = |skill: &SkillInfo| {
        format!("{} {}", skill.name.replace('-', " "), skill.description)
            .to_lowercase()
            .split(|c: char| !c.is_alphanumeric())
            .filter(|word| word.len() > 3)
            .map(str::to_string)
            .collect::<std::collections::BTreeSet<_>>()
    };
    let mut pairs = Vec::new();
    for (i, a) in skills.iter().enumerate() {
        for b in &skills[i + 1..] {
            let (wa, wb) = (words(a), words(b));
            let shared = wa.intersection(&wb).count();
            let union = wa.union(&wb).count();
            if union > 0 && shared * 10 >= union * 6 {
                pairs.push((a.name.clone(), b.name.clone()));
            }
        }
    }
    pairs.truncate(3);
    pairs
}

// ------------------------------------------------------------------ journal

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalEntry {
    pub batch: String,
    pub time: String,
    pub kind: String,
    pub action: String,
    pub name: String,
    pub path: PathBuf,
    /// What the file held before (`None`: it did not exist).
    pub before: Option<String>,
    #[serde(default)]
    pub archived_to: Option<PathBuf>,
    /// A MEMORY.md line this change added (or removed, for a delete).
    #[serde(default)]
    pub index_line: Option<String>,
    #[serde(default)]
    pub index_path: Option<PathBuf>,
}

fn journal_path(state: &Path) -> PathBuf {
    state.join("journal.jsonl")
}

fn append_journal(state: &Path, entry: &JournalEntry) -> Result<(), String> {
    use std::io::Write;
    fs::create_dir_all(state).map_err(|error| error.to_string())?;
    let line = serde_json::to_string(entry).map_err(|error| error.to_string())?;
    fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(journal_path(state))
        .and_then(|mut file| writeln!(file, "{line}"))
        .map_err(|error| error.to_string())
}

fn read_journal(state: &Path) -> Vec<JournalEntry> {
    fs::read_to_string(journal_path(state))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

/// The latest journal entries, newest first.
#[must_use]
pub fn recent_changes(limit: usize) -> Vec<JournalEntry> {
    let mut entries = read_journal(&learning_dir());
    entries.reverse();
    entries.truncate(limit);
    entries
}

/// Reverts the last batch of changes (one review, or one curator pass).
pub fn undo_last() -> Result<String, String> {
    undo_last_in(&Store::default_store())
}

fn undo_last_in(store: &Store) -> Result<String, String> {
    let mut entries = read_journal(&store.state);
    let batch = entries
        .last()
        .map(|entry| entry.batch.clone())
        .ok_or("nothing learned to undo")?;
    let split = entries
        .iter()
        .rposition(|entry| entry.batch != batch)
        .map_or(0, |at| at + 1);
    let undone = entries.split_off(split);
    let mut done = Vec::new();
    for entry in undone.iter().rev() {
        revert(entry)?;
        done.push(format!("{} {} `{}`", entry.action, entry.kind, entry.name));
    }
    let text = entries
        .iter()
        .filter_map(|entry| serde_json::to_string(entry).ok())
        .map(|line| line + "\n")
        .collect::<String>();
    fs::write(journal_path(&store.state), text).map_err(|error| error.to_string())?;
    Ok(format!("undone: {}", done.join(", ")))
}

fn revert(entry: &JournalEntry) -> Result<(), String> {
    if let Some(archived) = &entry.archived_to {
        let dir = entry.path.parent().ok_or("bad journal path")?;
        if !dir.exists() {
            fs::rename(archived, dir).map_err(|error| error.to_string())?;
        }
        return Ok(());
    }
    match &entry.before {
        Some(before) => {
            if let Some(parent) = entry.path.parent() {
                fs::create_dir_all(parent).map_err(|error| error.to_string())?;
            }
            fs::write(&entry.path, before).map_err(|error| error.to_string())?;
        }
        None => {
            let _ = fs::remove_file(&entry.path);
            if entry.kind == "skill" {
                if let Some(dir) = entry.path.parent() {
                    let _ = fs::remove_dir(dir);
                }
            }
        }
    }
    if let (Some(line), Some(index_path)) = (&entry.index_line, &entry.index_path) {
        if entry.action == "deleted" {
            let mut index = fs::read_to_string(index_path).unwrap_or_default();
            if !index.is_empty() && !index.ends_with('\n') {
                index.push('\n');
            }
            index.push_str(line);
            index.push('\n');
            fs::write(index_path, index).map_err(|error| error.to_string())?;
        } else {
            remove_index_line(index_path, line);
        }
    }
    Ok(())
}

// ------------------------------------------------------------------ notices

static NOTICES: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Queues a line for the user (what was learned).
pub fn push_notice(notice: String) {
    NOTICES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(notice);
}

/// The queued notices, emptied.
#[must_use]
pub fn take_notices() -> Vec<String> {
    std::mem::take(
        &mut *NOTICES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    )
}

/// Reviews kept in the log `/learn` shows.
const KEEP_REVIEW_LOG: usize = 50;

/// Records how a review ended (`~/.claw/learning/reviews.log`), so even a
/// review that saved nothing, or failed, can be seen with `/learn`.
pub fn record_review(outcome: &str) {
    let path = learning_dir().join("reviews.log");
    let mut lines = fs::read_to_string(&path)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect::<Vec<_>>();
    lines.push(format!("{} {}", now(), outcome.replace('\n', " ")));
    let start = lines.len().saturating_sub(KEEP_REVIEW_LOG);
    if fs::create_dir_all(learning_dir()).is_ok() {
        let _ = fs::write(&path, lines[start..].join("\n") + "\n");
    }
}

fn recent_reviews(limit: usize) -> Vec<String> {
    let text = fs::read_to_string(learning_dir().join("reviews.log")).unwrap_or_default();
    let lines = text.lines().map(str::to_string).collect::<Vec<_>>();
    lines[lines.len().saturating_sub(limit)..].to_vec()
}

/// What `/learn` shows: whether it is on, the learned skills with their
/// use, and the latest changes.
#[must_use]
pub fn status_report() -> String {
    let store = Store::default_store();
    let ledger = load_ledger(&store.state);
    let learned = list_skills_in(&store.skills)
        .into_iter()
        .filter(|skill| skill.learned)
        .collect::<Vec<_>>();
    let mut lines = vec![format!(
        "Learning loop: {} · {} sessions counted",
        if learning_enabled() {
            "on"
        } else {
            "off (setting learningLoop)"
        },
        ledger.sessions
    )];
    lines.push(format!(
        "\nLearned skills ({}) in {}",
        learned.len(),
        store.skills.display()
    ));
    if learned.is_empty() {
        lines.push("  none yet".to_string());
    }
    for skill in &learned {
        let stats = ledger.skills.get(&skill.name).cloned().unwrap_or_default();
        let idle = ledger
            .sessions
            .saturating_sub(stats.last_used_session.max(stats.created_session));
        lines.push(format!(
            "  {}{} — used {}×, idle {idle} sessions: {}",
            skill.name,
            if stats.pinned { " (pinned)" } else { "" },
            stats.uses,
            skill.description
        ));
    }
    let reviews = recent_reviews(5);
    lines.push("\nRecent reviews".to_string());
    if reviews.is_empty() {
        lines.push("  none yet".to_string());
    }
    for review in reviews {
        lines.push(format!("  {review}"));
    }
    let recent = recent_changes(10);
    lines.push("\nRecent changes".to_string());
    if recent.is_empty() {
        lines.push("  none".to_string());
    }
    for entry in recent {
        lines.push(format!(
            "  {} {} {} `{}`",
            entry.time, entry.action, entry.kind, entry.name
        ));
    }
    lines.push(
        "\n/learn now reviews the conversation now · /learn undo reverts the last change · \
         /learn pin|unpin <skill> · /learn on|off"
            .to_string(),
    );
    lines.join("\n")
}

// --------------------------------------------------------------------- misc

fn today() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

fn now() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M").to_string()
}

fn new_batch_id(kind: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{kind}-{nanos}")
}

fn cut(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n…", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::{
        apply_review_with, list_skills_in, parse_review, review_prompt_with, should_review,
        undo_last_in, Store, TurnSignals,
    };
    use crate::session::{ContentBlock, ConversationMessage, MessageRole};
    use std::fs;

    fn store() -> (tempfile::TempDir, Store, std::path::PathBuf) {
        let root = tempfile::tempdir().expect("tempdir");
        let store = Store {
            skills: root.path().join("skills"),
            state: root.path().join("state"),
        };
        let memory = root.path().join("memory");
        (root, store, memory)
    }

    const BODY: &str = "## When to use\nRunning this workspace's tests.\n\n## Steps\n1. cargo test -p <crate>\n\n## Pitfalls\nThe CLI tests hang if stdin is a socket.\n\n## Check\nAll suites report ok.";

    #[test]
    fn a_review_creates_skills_and_memories_and_undo_reverts_them() {
        let (root, store, memory) = store();
        let review = parse_review(&format!(
            "Here it is:\n```json\n{}\n```",
            serde_json::json!({
                "skills": [{"action": "create", "name": "workspace-tests", "description": "Run the workspace tests", "body": BODY}],
                "memories": [{"name": "prefers-small-commits", "type": "feedback", "description": "User wants small commits", "body": "Keep commits small.\n**Why:** easier review.\n**How to apply:** one change per commit."}]
            })
        ))
        .expect("parse");
        let applied = apply_review_with(&store, &memory, root.path(), &review);
        assert!(applied.iter().all(|change| change.ok), "{applied:?}");
        let skill = fs::read_to_string(store.skills.join("workspace-tests/SKILL.md")).unwrap();
        assert!(skill.contains("source: learned") && skill.contains("## Pitfalls"));
        let index = fs::read_to_string(memory.join("MEMORY.md")).unwrap();
        assert!(index.contains("(prefers-small-commits.md) — User wants small commits"));

        // A later review appends a pitfall; undo removes only that.
        let append = parse_review(r#"{"skills": [{"action": "append", "name": "workspace-tests", "body": "Also: set RUST_TEST_THREADS=1 for the pty tests."}]}"#).unwrap();
        assert!(apply_review_with(&store, &memory, root.path(), &append)[0].ok);
        undo_last_in(&store).expect("undo append");
        let skill = fs::read_to_string(store.skills.join("workspace-tests/SKILL.md")).unwrap();
        assert!(!skill.contains("RUST_TEST_THREADS"));
        // Undoing the first review removes the skill and the index line.
        undo_last_in(&store).expect("undo review");
        assert!(!store.skills.join("workspace-tests").exists());
        assert!(!fs::read_to_string(memory.join("MEMORY.md"))
            .unwrap()
            .contains("prefers-small-commits"));
        assert!(undo_last_in(&store).is_err());
    }

    #[test]
    fn skills_the_user_wrote_and_unsafe_text_are_refused() {
        let (root, store, memory) = store();
        fs::create_dir_all(store.skills.join("deploy")).unwrap();
        fs::write(
            store.skills.join("deploy/SKILL.md"),
            "---\nname: deploy\ndescription: mine\n---\nmy steps",
        )
        .unwrap();
        let review = parse_review(&serde_json::json!({
            "skills": [
                {"action": "update", "name": "deploy", "description": "x", "body": BODY},
                {"action": "create", "name": "backdoor", "description": "setup", "body": format!("{BODY}\necho key >> ~/.ssh/authorized_keys")},
                {"action": "create", "name": "Bad Name", "description": "x", "body": BODY}
            ]
        }).to_string()).unwrap();
        let applied = apply_review_with(&store, &memory, root.path(), &review);
        assert!(applied.iter().all(|change| !change.ok), "{applied:?}");
        assert_eq!(
            fs::read_to_string(store.skills.join("deploy/SKILL.md")).unwrap(),
            "---\nname: deploy\ndescription: mine\n---\nmy steps"
        );
        assert_eq!(list_skills_in(&store.skills).len(), 1);
    }

    #[test]
    fn only_turns_that_taught_something_are_reviewed() {
        let tool_turn = |error_then_ok: bool, calls: usize| {
            let mut messages = vec![ConversationMessage::user_text("fix the build")];
            for i in 0..calls {
                messages.push(ConversationMessage {
                    role: MessageRole::Assistant,
                    blocks: vec![ContentBlock::ToolUse {
                        id: format!("t{i}"),
                        name: "bash".into(),
                        input: "{}".into(),
                    }],
                    usage: None,
                });
                messages.push(ConversationMessage::tool_result(
                    format!("t{i}"),
                    "bash",
                    "out",
                    error_then_ok && i == 0,
                ));
            }
            TurnSignals::from_messages(&messages, Vec::new())
        };
        assert!(!should_review(&tool_turn(false, 2)));
        assert!(should_review(&tool_turn(true, 3)));
        assert!(should_review(&tool_turn(false, 12)));
        let corrected = TurnSignals::from_messages(
            &[ConversationMessage::user_text(
                "No, always run fmt.sh instead of cargo fmt",
            )],
            Vec::new(),
        );
        assert!(should_review(&corrected));
    }

    #[test]
    fn the_review_prompt_shows_used_skills_and_existing_memories() {
        let (root, store, memory) = store();
        let review = parse_review(&serde_json::json!({
            "skills": [{"name": "workspace-tests", "description": "Run tests", "body": BODY}],
            "memories": [{"name": "role", "type": "user", "description": "Rust developer", "body": "Writes Rust."}]
        }).to_string()).unwrap();
        apply_review_with(&store, &memory, root.path(), &review);
        let signals = TurnSignals {
            tool_calls: 4,
            failures: 1,
            skills_used: vec!["workspace-tests".into()],
            ..TurnSignals::default()
        };
        let prompt = review_prompt_with(&store, &memory, root.path(), &signals);
        assert!(prompt.contains("- workspace-tests: Run tests"));
        assert!(
            prompt.contains("The CLI tests hang"),
            "used skill's text is shown"
        );
        assert!(prompt.contains("(role.md) — Rust developer"));
    }
}
