//! Tool output too long for the conversation is saved to a file instead of
//! being cut and lost: the model gets the start and the end (where a build's
//! errors and a test run's summary are) and the file's path, and reads or
//! greps the rest when it needs it. Before this, bash kept only the first
//! 16 KiB, so a long build log lost exactly the errors at its end.
//!
//! Files go to `~/.claw/tool-output/` (outside the project, so nothing is
//! left in the repository); claw's file tools read there without asking.
//! Files older than a week are removed when a new one is saved.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

const KEEP_FOR: Duration = Duration::from_secs(7 * 24 * 3600);

/// The folder spilled output is saved in.
#[must_use]
pub fn spill_dir() -> PathBuf {
    crate::config::default_config_home().join("tool-output")
}

/// Whether `path` is inside [`spill_dir`].
#[must_use]
pub fn is_spill_path(path: &str) -> bool {
    let dir = spill_dir();
    let path = Path::new(path);
    let dir = fs::canonicalize(&dir).unwrap_or(dir);
    let path = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    path.starts_with(dir)
}

/// Saves `text` to a new file in the spill folder; its path, or `None` if
/// it could not be written.
pub fn save(text: &str, label: &str) -> Option<PathBuf> {
    save_in(&spill_dir(), text, label)
}

fn save_in(dir: &Path, text: &str, label: &str) -> Option<PathBuf> {
    fs::create_dir_all(dir).ok()?;
    remove_old(dir);
    let stamp = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let label = label
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>();
    let path = dir.join(format!(
        "{label}-{}-{:09}.txt",
        stamp.as_secs(),
        stamp.subsec_nanos()
    ));
    fs::write(&path, text).ok()?;
    Some(path)
}

fn remove_old(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let old = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age > KEEP_FOR);
        if old {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// `text` cut to about `max_bytes`: its first third and last two thirds,
/// with a note in between naming the file the whole of it was saved to.
/// Unchanged when it fits.
#[must_use]
pub fn spill_long(text: &str, max_bytes: usize, label: &str) -> String {
    spill_long_in(&spill_dir(), text, max_bytes, label)
}

fn spill_long_in(dir: &Path, text: &str, max_bytes: usize, label: &str) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let head_end = floor_char_boundary(text, max_bytes / 3);
    let tail_start = ceil_char_boundary(text, text.len() - (max_bytes - max_bytes / 3));
    let omitted = tail_start - head_end;
    let saved = save_in(dir, text, label).map_or_else(
        || "it could not be saved".to_string(),
        |path| {
            format!(
                "the whole output ({} bytes) is in {}: read it with read_file (offset/limit) \
                 or search it with grep_search",
                text.len(),
                path.display()
            )
        },
    );
    format!(
        "{}\n\n[… {omitted} bytes omitted here; {saved} …]\n\n{}",
        &text[..head_end],
        &text[tail_start..]
    )
}

fn floor_char_boundary(text: &str, mut at: usize) -> usize {
    while at > 0 && !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

fn ceil_char_boundary(text: &str, mut at: usize) -> usize {
    while at < text.len() && !text.is_char_boundary(at) {
        at += 1;
    }
    at
}

#[cfg(test)]
mod tests {
    use super::spill_long_in;

    #[test]
    fn long_output_keeps_its_start_and_end_and_is_saved_whole() {
        let home = std::env::temp_dir().join(format!("claw-spill-{}", std::process::id()));
        let text = format!("START\n{}\nerror: the end", "x".repeat(50_000));
        let cut = spill_long_in(&home, &text, 3_000, "bash");
        assert!(cut.len() < 3_400, "{}", cut.len());
        assert!(cut.starts_with("START"));
        assert!(cut.ends_with("error: the end"));
        let path = cut
            .split(" is in ")
            .nth(1)
            .and_then(|rest| rest.split(": read it").next())
            .expect("path in note");
        assert_eq!(std::fs::read_to_string(path).unwrap(), text);
        assert_eq!(spill_long_in(&home, "short", 3_000, "bash"), "short");
        let _ = std::fs::remove_dir_all(home);
    }
}
