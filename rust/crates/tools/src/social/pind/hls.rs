//! Turns a Pinterest HLS master playlist into one `.mp4`. Video story pins
//! (verified live) and any video pin with no progressive `.mp4` are served
//! *only* this way: a master playlist naming per-resolution video
//! variants plus a separate audio rendition. Each variant/rendition's media
//! playlist was observed to be a single fragmented-MP4 file (`.cmfv` /
//! `.cmfa`) addressed by `#EXT-X-BYTERANGE`s that cover it end to end, so
//! rather than reassembling segments this module fetches each track's one
//! file whole and lets `ffmpeg` stream-copy the two into a single `.mp4`.
//! A playlist that doesn't have that single-file shape is refused, not
//! guessed at.
//!
//! **Subprocess exception** (`CLAUDE.md` §9, the fifth): `ffmpeg` is run
//! with the same rigor as the others. Absolute path (`FFMPEG_BIN`, never
//! `$PATH`-resolved); a closed argv in which the only variable tokens are
//! three paths this module builds itself inside a fresh private temp
//! directory from fixed names — no remote or message-derived text reaches
//! argv; `tokio::process::Command` with no shell. Beyond that:
//! `-protocol_whitelist file` (before every input) so ffmpeg can never open
//! a network URL or any non-file protocol itself, `-f mp4` forcing the
//! demuxer so a hostile "track" can't be sniffed as a playlist format that
//! references other files, `-c copy` (no decode/encode of untrusted
//! media), `-nostdin`, a hard timeout with kill-on-drop, and the temp
//! directory removed on every path. All *network* access stays in
//! `super::net` (host allowlist, IP pinning, size caps); `ffmpeg` only ever
//! sees bytes this module already downloaded and capped.

use std::path::Path;
use std::process::Stdio;

use reqwest::Url;
use tokio::process::Command;

use super::net::fetch_capped;
use super::types::{FFMPEG_TIMEOUT, MAX_HLS_TRACK_BYTES, MAX_MEDIA_BYTES, MAX_PLAYLIST_BYTES};

const FFMPEG_BIN: &str = "/usr/bin/ffmpeg";

/// The value of attribute `key` (e.g. `URI`) in an HLS attribute list, for
/// the quoted form this module needs.
fn quoted_attr<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!("{key}=\"");
    let start = line.find(&needle)? + needle.len();
    let rest = &line[start..];
    Some(&rest[..rest.find('"')?])
}

fn resolution_area(line: &str) -> u64 {
    let Some(start) = line.find("RESOLUTION=") else {
        return 0;
    };
    let rest = &line[start + "RESOLUTION=".len()..];
    let end = rest
        .find(|c: char| c != 'x' && !c.is_ascii_digit())
        .unwrap_or(rest.len());
    let Some((w, h)) = rest[..end].split_once('x') else {
        return 0;
    };
    w.parse::<u64>()
        .unwrap_or(0)
        .saturating_mul(h.parse::<u64>().unwrap_or(0))
}

/// From a master playlist: the largest-resolution video variant's playlist
/// URL, and the first audio rendition's, if any. A playlist that is itself
/// a media playlist (has `#EXTINF`, no variants) is returned as the video
/// with no audio.
pub(crate) fn parse_master(text: &str, base: &Url) -> Result<(Url, Option<Url>), String> {
    let mut best: Option<(u64, String)> = None;
    let mut audio: Option<String> = None;
    let mut lines = text.lines().map(str::trim);
    while let Some(line) = lines.next() {
        if line.starts_with("#EXT-X-STREAM-INF:") {
            let area = resolution_area(line);
            let uri = lines
                .by_ref()
                .find(|l| !l.is_empty() && !l.starts_with('#'));
            if let Some(uri) = uri {
                if best.as_ref().is_none_or(|(a, _)| area >= *a) {
                    best = Some((area, uri.to_string()));
                }
            }
        } else if line.starts_with("#EXT-X-MEDIA:")
            && line.contains("TYPE=AUDIO")
            && audio.is_none()
        {
            audio = quoted_attr(line, "URI").map(str::to_string);
        }
    }

    let join = |u: &str| base.join(u).map_err(|_| "invalid playlist URL".to_string());
    match best {
        Some((_, uri)) => Ok((join(&uri)?, audio.as_deref().map(join).transpose()?)),
        None if text.contains("#EXTINF") => Ok((base.clone(), None)),
        None => Err("unrecognized HLS playlist".to_string()),
    }
}

/// From a media playlist: the one file every `#EXT-X-MAP`/segment URI points
/// at. Anything but exactly one distinct file is refused (see module doc).
pub(crate) fn single_track_file(text: &str, base: &Url) -> Result<Url, String> {
    let mut uris: Vec<&str> = Vec::new();
    for line in text.lines().map(str::trim) {
        let uri = if line.starts_with("#EXT-X-MAP:") {
            quoted_attr(line, "URI")
        } else if !line.is_empty() && !line.starts_with('#') {
            Some(line)
        } else {
            None
        };
        if let Some(u) = uri {
            if !uris.contains(&u) {
                uris.push(u);
            }
        }
    }
    match uris.as_slice() {
        [one] => base
            .join(one)
            .map_err(|_| "invalid playlist URL".to_string()),
        _ => Err("unsupported HLS layout (not a single-file track)".to_string()),
    }
}

async fn fetch_playlist(url: &Url) -> Result<String, String> {
    let bytes = fetch_capped(url.as_str(), MAX_PLAYLIST_BYTES, "playlist").await?;
    String::from_utf8(bytes).map_err(|_| "unrecognized HLS playlist".to_string())
}

/// Downloads the master playlist at `master_url`, both tracks, and muxes
/// them into one in-memory `.mp4`.
pub async fn download_hls(master_url: &str) -> Result<Vec<u8>, String> {
    let master_url = Url::parse(master_url).map_err(|_| "invalid HLS URL".to_string())?;
    let (video_pl, audio_pl) = parse_master(&fetch_playlist(&master_url).await?, &master_url)?;

    let video_file = if video_pl == master_url {
        // The "master" was itself a media playlist; reuse the text we have.
        single_track_file(&fetch_playlist(&master_url).await?, &master_url)?
    } else {
        single_track_file(&fetch_playlist(&video_pl).await?, &video_pl)?
    };
    let video = fetch_capped(video_file.as_str(), MAX_HLS_TRACK_BYTES, "video track").await?;

    let audio = match audio_pl {
        Some(pl) => {
            let file = single_track_file(&fetch_playlist(&pl).await?, &pl)?;
            Some(fetch_capped(file.as_str(), MAX_HLS_TRACK_BYTES, "audio track").await?)
        }
        None => None,
    };

    mux(&video, audio.as_deref()).await
}

/// Stream-copies the tracks into one `.mp4` via `ffmpeg`, inside a private
/// temp directory that is always removed afterwards.
async fn mux(video: &[u8], audio: Option<&[u8]>) -> Result<Vec<u8>, String> {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let dir = std::env::temp_dir().join(format!("pind-{}-{unique}", std::process::id()));
    {
        let mut b = std::fs::DirBuilder::new();
        std::os::unix::fs::DirBuilderExt::mode(&mut b, 0o700);
        b.create(&dir)
            .map_err(|e| format!("creating temp dir: {e}"))?;
    }
    let result = mux_in(&dir, video, audio).await;
    if let Err(e) = tokio::fs::remove_dir_all(&dir).await {
        tracing::warn!(error = %e, "pind: failed to remove temp dir");
    }
    result
}

async fn mux_in(dir: &Path, video: &[u8], audio: Option<&[u8]>) -> Result<Vec<u8>, String> {
    let (v_path, a_path, out_path) = (dir.join("v.mp4"), dir.join("a.mp4"), dir.join("out.mp4"));
    tokio::fs::write(&v_path, video)
        .await
        .map_err(|e| format!("writing temp file: {e}"))?;

    let mut cmd = Command::new(FFMPEG_BIN);
    cmd.args([
        "-nostdin",
        "-hide_banner",
        "-loglevel",
        "error",
        "-protocol_whitelist",
        "file",
        "-f",
        "mp4",
        "-i",
    ])
    .arg(&v_path);
    if let Some(audio) = audio {
        tokio::fs::write(&a_path, audio)
            .await
            .map_err(|e| format!("writing temp file: {e}"))?;
        cmd.args(["-protocol_whitelist", "file", "-f", "mp4", "-i"])
            .arg(&a_path);
        cmd.args(["-map", "0:v:0", "-map", "1:a:0"]);
    } else {
        cmd.args(["-map", "0:v:0"]);
    }
    cmd.args(["-c", "copy", "-movflags", "+faststart", "-f", "mp4", "-y"])
        .arg(&out_path);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let output = tokio::time::timeout(FFMPEG_TIMEOUT, cmd.output())
        .await
        .map_err(|_| "video mux timed out".to_string())?
        .map_err(|e| format!("could not run ffmpeg: {e}"))?;
    if !output.status.success() {
        tracing::warn!(stderr = %String::from_utf8_lossy(&output.stderr).trim(), "pind: ffmpeg mux failed");
        return Err("video mux failed".to_string());
    }

    let meta = tokio::fs::metadata(&out_path)
        .await
        .map_err(|_| "video mux produced no output".to_string())?;
    if meta.len() == 0 || meta.len() > MAX_MEDIA_BYTES {
        return Err("video mux produced an unusable file".to_string());
    }
    tokio::fs::read(&out_path)
        .await
        .map_err(|e| format!("reading muxed video: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Captured from a real video story pin's HLS master/media playlists.
    const MASTER: &str = "#EXTM3U\n#EXT-X-VERSION:6\n#EXT-X-INDEPENDENT-SEGMENTS\n\n\
        #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"audio1\",NAME=\"1\",AUTOSELECT=YES,URI=\"s_audio.m3u8\",CHANNELS=\"2\"\n\
        #EXT-X-STREAM-INF:BANDWIDTH=245407,RESOLUTION=234x416,AUDIO=\"audio1\"\ns_240w.m3u8\n\n\
        #EXT-X-STREAM-INF:BANDWIDTH=952043,CODECS=\"avc1.64081F,mp4a.40.29\",RESOLUTION=576x1024,AUDIO=\"audio1\"\ns_720w.m3u8\n\n\
        #EXT-X-STREAM-INF:BANDWIDTH=753447,RESOLUTION=540x960,AUDIO=\"audio1\"\ns_540w.m3u8\n";
    const MEDIA: &str = "#EXTM3U\n#EXT-X-VERSION:6\n#EXT-X-MAP:URI=\"s_720w.cmfv\",BYTERANGE=\"1034@0\"\n\
        #EXTINF:2\n#EXT-X-BYTERANGE:211264@1278\ns_720w.cmfv\n#EXTINF:2\n#EXT-X-BYTERANGE:230139@212542\ns_720w.cmfv\n#EXT-X-ENDLIST\n";

    fn base() -> Url {
        Url::parse("https://v1.pinimg.com/videos/iht/hls/7d/6b/69/s.m3u8").unwrap()
    }

    #[test]
    fn master_picks_largest_variant_and_audio_rendition() {
        let (v, a) = parse_master(MASTER, &base()).unwrap();
        assert_eq!(
            v.as_str(),
            "https://v1.pinimg.com/videos/iht/hls/7d/6b/69/s_720w.m3u8"
        );
        assert_eq!(
            a.unwrap().as_str(),
            "https://v1.pinimg.com/videos/iht/hls/7d/6b/69/s_audio.m3u8"
        );
    }

    #[test]
    fn master_without_variants_but_with_segments_is_a_video_only_media_playlist() {
        let (v, a) = parse_master(MEDIA, &base()).unwrap();
        assert_eq!(v, base());
        assert!(a.is_none());
    }

    #[test]
    fn master_garbage_is_refused() {
        assert!(parse_master("<html>nope</html>", &base()).is_err());
    }

    #[test]
    fn single_file_track_resolves_map_and_segments_to_one_file() {
        let u = single_track_file(MEDIA, &base()).unwrap();
        assert_eq!(
            u.as_str(),
            "https://v1.pinimg.com/videos/iht/hls/7d/6b/69/s_720w.cmfv"
        );
    }

    #[test]
    fn multi_file_track_is_refused() {
        let text = "#EXTM3U\n#EXTINF:2\nseg1.ts\n#EXTINF:2\nseg2.ts\n";
        assert!(single_track_file(text, &base()).is_err());
    }

    #[test]
    fn hostile_segment_uri_still_resolves_but_is_left_to_the_host_allowlist() {
        // Joining never fabricates a trusted host: an absolute foreign URL
        // stays foreign, and `net::guarded_get` is what rejects it.
        let text = "#EXTM3U\n#EXTINF:2\nhttp://169.254.169.254/x\n";
        assert_eq!(
            single_track_file(text, &base()).unwrap().host_str(),
            Some("169.254.169.254")
        );
    }
}
