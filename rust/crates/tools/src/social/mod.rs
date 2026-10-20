//! The `SocialMedia` tool: saves the images and videos of one Instagram,
//! Facebook or Pinterest post, given its link. Models asked for pictures
//! from these sites spent many steps hunting for CDN addresses (and
//! guessed ones that did not exist); these scrapers already know each
//! site's own web API.
//!
//! `igd`, `fbd` and `pind` are ported from the herald service
//! (`piNas/projs/rust/server/ore`), with their SSRF hardening intact: every
//! request is limited to the platform's own hosts, resolved and pinned to a
//! public address (`is_blocked_ip`), and size-capped. This module adds what
//! they used from the rest of that crate (the IP blocklist, Netscape cookie
//! loading) and the tool around them.
//!
//! Cookies: a logged-in session is optional. `~/.claw/cookies/<platform>.txt`
//! (Netscape format, as a browser "export cookies" extension writes it) is
//! used when present; Instagram needs one for private or age-gated posts.

pub mod fbd;
pub mod igd;
pub mod pind;

use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Debug, Deserialize)]
pub(crate) struct SocialMediaInput {
    /// A post, reel, video or pin link (share and short links work).
    #[serde(alias = "link")]
    url: String,
    /// Directory to save into; default `~/Downloads/claw-social/<platform>`.
    #[serde(alias = "save_to", alias = "dir")]
    dest: Option<String>,
    /// For an Instagram profile: how many of its latest posts (default 6).
    #[serde(alias = "limit", alias = "posts")]
    count: Option<usize>,
}

/// Most posts one profile request downloads.
const MAX_PROFILE_POSTS: usize = 24;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Platform {
    Instagram,
    Facebook,
    Pinterest,
}

impl Platform {
    fn name(self) -> &'static str {
        match self {
            Platform::Instagram => "instagram",
            Platform::Facebook => "facebook",
            Platform::Pinterest => "pinterest",
        }
    }
}

fn platform_for(url: &str) -> Option<Platform> {
    let host = reqwest::Url::parse(url.trim())
        .ok()?
        .host_str()?
        .to_ascii_lowercase();
    let is = |domain: &str| host == domain || host.ends_with(&format!(".{domain}"));
    if is("instagram.com") || is("instagr.am") {
        Some(Platform::Instagram)
    } else if is("facebook.com") || is("fb.watch") || is("fb.com") {
        Some(Platform::Facebook)
    } else if is("pin.it") || host.split('.').any(|label| label == "pinterest") {
        Some(Platform::Pinterest)
    } else {
        None
    }
}

/// One downloaded item, whatever the platform.
struct Saved {
    bytes: Vec<u8>,
    video: bool,
    extension: &'static str,
}

pub(crate) fn run(input: SocialMediaInput) -> Result<Value, String> {
    let url =
        instagram_handle_url(input.url.trim()).unwrap_or_else(|| input.url.trim().to_string());
    if platform_for(&url) == Some(Platform::Instagram)
        && igd::utils::extract_shortcode(&url).is_none()
    {
        return run_instagram_profile(&url, input.dest.as_deref(), input.count);
    }
    let platform = platform_for(&url).ok_or_else(|| {
        "not an Instagram, Facebook or Pinterest link; for other sites use WebFetch (it lists \
         a page's images) and Download"
            .to_string()
    })?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    let items = runtime.block_on(fetch(platform, &url))?;
    if items.is_empty() {
        return Err("the post has no images or videos that could be fetched".to_string());
    }

    let dir = input.dest.as_deref().map_or_else(
        || home().join("Downloads/claw-social").join(platform.name()),
        expand_home,
    );
    std::fs::create_dir_all(&dir).map_err(|error| format!("{}: {error}", dir.display()))?;
    let stem = format!("{}-{}", platform.name(), post_id(&url));
    let mut files = Vec::new();
    let mut show = Vec::new();
    for (index, item) in items.iter().enumerate() {
        let extension = sniffed_extension(&item.bytes).unwrap_or(item.extension);
        let path = free_path(&dir, &format!("{stem}-{}", index + 1), extension);
        std::fs::write(&path, &item.bytes)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        let kind = if item.video { "video" } else { "image" };
        if !item.video {
            show.push(format!(
                "![{} {}]({})",
                platform.name(),
                index + 1,
                path.display()
            ));
        }
        files.push(
            json!({"path": path.display().to_string(), "kind": kind, "bytes": item.bytes.len()}),
        );
    }
    Ok(json!({
        "platform": platform.name(),
        "saved": files,
        "show": show,
        "note": "To show the images, put the `show` lines in your reply as they are. Videos are saved files; give their paths.",
    }))
}

/// `@name` or a bare `instagram.com/name` as a full profile address.
fn instagram_handle_url(text: &str) -> Option<String> {
    let name = text.strip_prefix('@').unwrap_or(text);
    if let Some(rest) = name
        .strip_prefix("instagram.com/")
        .or_else(|| name.strip_prefix("www.instagram.com/"))
    {
        return Some(format!("https://www.instagram.com/{rest}"));
    }
    (text.starts_with('@')
        && !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_'))
    .then(|| format!("https://www.instagram.com/{name}/"))
}

/// An Instagram profile: its latest posts' links come from the page in
/// claw's browser (Instagram's own profile API now wants a login; the page
/// still lists the grid to a logged-out browser), then each post is fetched
/// as a single post. A user asked for "the images of user X"; the model
/// fought the login dialog in the browser and gave up.
fn run_instagram_profile(
    url: &str,
    dest: Option<&str>,
    count: Option<usize>,
) -> Result<Value, String> {
    let wanted = count.unwrap_or(6).clamp(1, MAX_PROFILE_POSTS);
    let posts = instagram_profile_posts(url, wanted)?;
    if posts.is_empty() {
        return Err(
            "no posts found on that profile (private, empty, or Instagram showed a \
                    login wall); with a cookie file at ~/.claw/cookies/instagram.txt, try \
                    single post links"
                .to_string(),
        );
    }
    let mut saved = Vec::new();
    let mut show = Vec::new();
    let mut failed = Vec::new();
    for post in &posts {
        match run(SocialMediaInput {
            url: post.clone(),
            dest: dest.map(str::to_string),
            count: None,
        }) {
            Ok(result) => {
                saved.extend(result["saved"].as_array().cloned().unwrap_or_default());
                show.extend(result["show"].as_array().cloned().unwrap_or_default());
            }
            Err(error) => {
                let rate_limited = error.contains("rate-limited");
                failed.push(json!({"post": post, "error": error}));
                if rate_limited {
                    break;
                }
            }
        }
    }
    Ok(json!({
        "platform": "instagram",
        "profile": url,
        "posts": posts,
        "saved": saved,
        "show": show,
        "failed": failed,
        "note": "To show the images, put the `show` lines in your reply as they are. Videos are saved files; give their paths.",
    }))
}

/// The newest `wanted` post and reel links on an Instagram profile page,
/// read in claw's browser (scrolling for more when needed).
fn instagram_profile_posts(url: &str, wanted: usize) -> Result<Vec<String>, String> {
    let browser = |input: Value| -> Result<Value, String> {
        let input = serde_json::from_value(input).map_err(|error| error.to_string())?;
        crate::browser::run(input)
    };
    browser(json!({"action": "open", "url": url, "headless": true}))?;
    let collect = r"Array.from(new Set(Array.from(document.querySelectorAll('a[href]')).map(a => a.href.split('?')[0]).filter(h => /instagram\.com\/([^/]+\/)?(p|reel)\/[A-Za-z0-9_-]+/.test(h))))";
    let mut posts: Vec<String> = Vec::new();
    for _ in 0..6 {
        let found = browser(json!({"action": "eval", "script": collect}))?;
        let before = posts.len();
        for link in found["value"].as_array().into_iter().flatten() {
            if let Some(link) = link.as_str() {
                if !posts.iter().any(|known| known == link) {
                    posts.push(link.to_string());
                }
            }
        }
        if posts.len() >= wanted || (posts.len() == before && before > 0) {
            break;
        }
        browser(json!({"action": "scroll", "direction": "down"}))?;
        std::thread::sleep(std::time::Duration::from_millis(1500));
    }
    posts.truncate(wanted);
    Ok(posts)
}

async fn fetch(platform: Platform, url: &str) -> Result<Vec<Saved>, String> {
    let cookies = std::fs::read_to_string(
        home()
            .join(".claw/cookies")
            .join(format!("{}.txt", platform.name())),
    )
    .ok();
    Ok(match platform {
        Platform::Instagram => {
            let cache = home().join(".claw/cache/igd_operation_cache.json");
            if let Some(parent) = cache.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            igd::download_post(&cache, url, cookies.as_deref())
                .await?
                .into_iter()
                .map(|item| Saved {
                    video: item.media_type == igd::types::MediaType::Video,
                    extension: item.media_type.extension(),
                    bytes: item.bytes,
                })
                .collect()
        }
        Platform::Facebook => fbd::download_post(url)
            .await?
            .into_iter()
            .map(|item| Saved {
                video: item.media_type == fbd::types::MediaType::Video,
                extension: item.media_type.extension(),
                bytes: item.bytes,
            })
            .collect(),
        Platform::Pinterest => pind::download_post(url)
            .await?
            .into_iter()
            .map(|item| Saved {
                video: item.media_type == pind::types::MediaType::Video,
                extension: item.extension,
                bytes: item.bytes,
            })
            .collect(),
    })
}

/// The file type the bytes really are, by their first bytes; the
/// platform's own claim only decides what was worth fetching.
fn sniffed_extension(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("jpg")
    } else if bytes.starts_with(b"\x89PNG") {
        Some("png")
    } else if bytes.starts_with(b"GIF8") {
        Some("gif")
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        Some("webp")
    } else if bytes.get(4..8) == Some(b"ftyp") {
        Some("mp4")
    } else {
        None
    }
}

/// A short id for file names: the last non-empty path segment, kept to
/// safe characters.
fn post_id(url: &str) -> String {
    let id = reqwest::Url::parse(url)
        .ok()
        .and_then(|parsed| {
            parsed
                .path_segments()?
                .filter(|segment| !segment.is_empty())
                .last()
                .map(str::to_string)
        })
        .unwrap_or_default()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(40)
        .collect::<String>();
    if id.is_empty() {
        "post".to_string()
    } else {
        id
    }
}

/// `dir/stem.ext`, or `dir/stem (1).ext`, … if that exists.
fn free_path(dir: &Path, stem: &str, extension: &str) -> PathBuf {
    let mut path = dir.join(format!("{stem}.{extension}"));
    let mut n = 1;
    while path.exists() {
        path = dir.join(format!("{stem} ({n}).{extension}"));
        n += 1;
    }
    path
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from)
}

fn expand_home(path: &str) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => home().join(rest),
        None if path == "~" => home(),
        None => PathBuf::from(path),
    }
}

// ─── From herald's `ore::net_fetch` and `ore::platform_cookies` ───

fn is_ipv4_blocked(v4: &std::net::Ipv4Addr) -> bool {
    if v4.is_private()
        || v4.is_loopback()
        || v4.is_link_local()
        || v4.is_multicast()
        || v4.is_broadcast()
        || v4.is_documentation()
        || v4.is_unspecified()
    {
        return true;
    }
    let o = v4.octets();
    o[0] == 100 && (64..=127).contains(&o[1])
}

/// Unique-local (`fc00::/7`) and link-local (`fe80::/10`) by their leading
/// bits; an IPv4-mapped address is checked as the IPv4 address it holds.
fn is_ipv6_blocked(v6: &std::net::Ipv6Addr) -> bool {
    if v6.is_loopback() || v6.is_multicast() || v6.is_unspecified() {
        return true;
    }
    let seg0 = v6.segments()[0];
    if (seg0 & 0xfe00) == 0xfc00 || (seg0 & 0xffc0) == 0xfe80 {
        return true;
    }
    if let Some(v4) = v6.to_ipv4_mapped() {
        return is_ipv4_blocked(&v4);
    }
    false
}

/// A private, loopback, link-local, or otherwise non-public address: the
/// scrapers refuse to connect to one, whatever URL led there.
pub fn is_blocked_ip(ip: &std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => is_ipv4_blocked(v4),
        std::net::IpAddr::V6(v6) => is_ipv6_blocked(v6),
    }
}

/// `(domain, path, name, value)` from Netscape cookie-jar text; comments,
/// blank and malformed lines are skipped.
fn parse_netscape_cookies(text: &str) -> Vec<(String, String, String, String)> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| {
            let fields: Vec<&str> = line.split('\t').collect();
            if fields.len() != 7 {
                return None;
            }
            Some((
                fields[0].trim_start_matches('.').to_string(),
                fields[2].to_string(),
                fields[5].to_string(),
                fields[6].to_string(),
            ))
        })
        .collect()
}

/// Adds the cookies in `cookies_text` whose domain matches `target_host` (or
/// is a parent of it) to `jar`, before the first request goes out.
pub fn seed_jar(jar: &reqwest::cookie::Jar, cookies_text: &str, target_host: &str) {
    for (domain, path, name, value) in parse_netscape_cookies(cookies_text) {
        let host_matches = target_host == domain || target_host.ends_with(&format!(".{domain}"));
        if !host_matches {
            continue;
        }
        let Ok(url) = reqwest::Url::parse(&format!("https://{target_host}{path}")) else {
            continue;
        };
        jar.add_cookie_str(
            &format!("{name}={value}; Domain={domain}; Path={path}"),
            &url,
        );
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    pub struct TempDir {
        pub path: PathBuf,
    }

    impl TempDir {
        pub fn new(label: &str) -> Self {
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "claw-social-test-{label}-{}-{n}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).expect("create temp test dir");
            TempDir { path }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    impl std::ops::Deref for TempDir {
        type Target = Path;
        fn deref(&self) -> &Path {
            &self.path
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{is_blocked_ip, platform_for, post_id, sniffed_extension, Platform};

    #[test]
    fn links_go_to_their_platform() {
        assert_eq!(
            platform_for("https://www.instagram.com/p/C6xptGZrBRS/"),
            Some(Platform::Instagram)
        );
        assert_eq!(
            platform_for("https://www.facebook.com/share/r/abc/"),
            Some(Platform::Facebook)
        );
        assert_eq!(
            platform_for("https://fb.watch/xyz/"),
            Some(Platform::Facebook)
        );
        assert_eq!(
            platform_for("https://pin.it/3abc"),
            Some(Platform::Pinterest)
        );
        assert_eq!(
            platform_for("https://in.pinterest.com/pin/123/"),
            Some(Platform::Pinterest)
        );
        assert_eq!(platform_for("https://evilinstagram.com/p/x/"), None);
        assert_eq!(platform_for("https://wallhaven.cc/w/abc"), None);
        assert_eq!(
            super::instagram_handle_url("@i._.hammock").as_deref(),
            Some("https://www.instagram.com/i._.hammock/")
        );
        assert_eq!(
            super::instagram_handle_url("instagram.com/i._.hammock").as_deref(),
            Some("https://www.instagram.com/i._.hammock")
        );
        assert_eq!(super::instagram_handle_url("https://x.com/a"), None);
        assert_eq!(
            post_id("https://www.instagram.com/p/C6xptGZrBRS/"),
            "C6xptGZrBRS"
        );
        assert_eq!(sniffed_extension(b"\x89PNG\r\n"), Some("png"));
        assert!(is_blocked_ip(&"127.0.0.1".parse().unwrap()));
        assert!(is_blocked_ip(&"::ffff:10.0.0.1".parse().unwrap()));
        assert!(!is_blocked_ip(&"157.240.1.1".parse().unwrap()));
    }
}

#[cfg(test)]
mod live {
    /// `CLAW_SOCIAL_TEST_URL=<post link> cargo test -p tools live_social -- --ignored --nocapture`
    #[test]
    #[ignore = "fetches a real post over the network"]
    fn live_social() {
        let url = std::env::var("CLAW_SOCIAL_TEST_URL").expect("CLAW_SOCIAL_TEST_URL");
        let dest = std::env::temp_dir().join("claw-social-live");
        let result = super::run(super::SocialMediaInput {
            url,
            dest: Some(dest.display().to_string()),
            count: Some(3),
        });
        println!("{result:#?}");
        assert!(result.is_ok());
    }
}
