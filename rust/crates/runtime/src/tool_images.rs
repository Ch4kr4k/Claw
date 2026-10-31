//! Images a tool hands to the model (a screenshot, say). Tool results are
//! text, so a tool names a picture with a line `claw-image: /path.png` in its
//! output; the image is then attached to the tool result message as vision
//! input. Only the newest few are kept in the conversation: every request
//! resends them, and an old screenshot is rarely worth its tokens.

use crate::session::{ContentBlock, ConversationMessage, MessageRole};

/// Starts the output line that names an image file.
pub const TOOL_IMAGE_MARKER: &str = "claw-image: ";
/// Tool images kept in the conversation; older ones are dropped.
pub const KEEP_TOOL_IMAGES: usize = 2;
/// Larger files are not attached.
const MAX_IMAGE_BYTES: usize = 8 * 1024 * 1024;
/// Estimated tokens one tool result may carry, text and images together
/// (after deepseek-harness's shared spill budget): a result naming many
/// screenshots no longer sends them all. `CLAW_TOOL_RESULT_MAX_CHARS`
/// (a quarter of it, in tokens) lowers it.
const DEFAULT_RESULT_TOKEN_BUDGET: usize = 25_000;
/// What an image costs when its size cannot be read: the most a provider
/// charges after scaling it down.
const MAX_IMAGE_TOKENS: usize = 1_600;

fn result_token_budget() -> usize {
    std::env::var("CLAW_TOOL_RESULT_MAX_CHARS")
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|chars| *chars > 0)
        .map_or(DEFAULT_RESULT_TOKEN_BUDGET, |chars| {
            (chars / 4).max(MAX_IMAGE_TOKENS)
        })
}

/// Estimated tokens for an image: width × height / 750 (Anthropic's rule),
/// at most [`MAX_IMAGE_TOKENS`]; the maximum when the size is unknown.
fn image_tokens(bytes: &[u8]) -> usize {
    image_size(bytes).map_or(MAX_IMAGE_TOKENS, |(width, height)| {
        ((width * height) / 750).clamp(1, MAX_IMAGE_TOKENS)
    })
}

/// Width and height of a PNG, GIF or JPEG.
fn image_size(bytes: &[u8]) -> Option<(usize, usize)> {
    let be = |at: usize| Some(u32::from_be_bytes(bytes.get(at..at + 4)?.try_into().ok()?) as usize);
    let be16 =
        |at: usize| Some(u16::from_be_bytes(bytes.get(at..at + 2)?.try_into().ok()?) as usize);
    if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        return Some((be(16)?, be(20)?));
    }
    if bytes.starts_with(b"GIF8") {
        let le16 =
            |at: usize| Some(u16::from_le_bytes(bytes.get(at..at + 2)?.try_into().ok()?) as usize);
        return Some((le16(6)?, le16(8)?));
    }
    if bytes.starts_with(&[0xFF, 0xD8]) {
        let mut at = 2;
        while at + 9 < bytes.len() {
            if bytes[at] != 0xFF {
                at += 1;
                continue;
            }
            let marker = bytes[at + 1];
            if matches!(marker, 0xC0..=0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF) {
                return Some((be16(at + 7)?, be16(at + 5)?));
            }
            at += 2 + be16(at + 2)?;
        }
    }
    None
}

/// Attaches the images that `message`'s tool output names.
pub fn attach_tool_images(message: &mut ConversationMessage) {
    let paths: Vec<String> = message
        .blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolResult { output, .. } => Some(output),
            _ => None,
        })
        .flat_map(|output| output.lines())
        .filter_map(|line| line.trim().strip_prefix(TOOL_IMAGE_MARKER))
        .map(|path| path.trim().to_string())
        .collect();
    let text_tokens: usize = message
        .blocks
        .iter()
        .map(|block| match block {
            ContentBlock::ToolResult { output, .. } => output.len() / 4,
            _ => 0,
        })
        .sum();
    // The first image always goes; later ones while the budget lasts.
    let mut room = result_token_budget().saturating_sub(text_tokens);
    let mut left_out = Vec::new();
    for path in paths {
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let Some(media_type) = media_type(&bytes) else {
            continue;
        };
        if bytes.len() > MAX_IMAGE_BYTES {
            continue;
        }
        let cost = image_tokens(&bytes);
        let attached = message
            .blocks
            .iter()
            .any(|block| matches!(block, ContentBlock::Image { .. }));
        if attached && cost > room {
            left_out.push(path);
            continue;
        }
        room = room.saturating_sub(cost);
        message.blocks.push(ContentBlock::Image {
            media_type: media_type.to_string(),
            data: base64(&bytes),
            source: path,
        });
    }
    if !left_out.is_empty() {
        if let Some(ContentBlock::ToolResult { output, .. }) = message
            .blocks
            .iter_mut()
            .find(|block| matches!(block, ContentBlock::ToolResult { .. }))
        {
            output.push_str(&format!(
                "\n[{} more image(s) not attached: over this result's size budget. To see one, \
                 read it with read_file: {}]",
                left_out.len(),
                left_out.join(", ")
            ));
        }
    }
}

/// Removes all but the newest `keep` images that tools attached.
pub fn drop_old_tool_images(messages: &mut [ConversationMessage], keep: usize) {
    let mut seen = 0;
    for message in messages.iter_mut().rev() {
        if message.role != MessageRole::Tool {
            continue;
        }
        let mut index = message.blocks.len();
        while index > 0 {
            index -= 1;
            if matches!(message.blocks[index], ContentBlock::Image { .. }) {
                seen += 1;
                if seen > keep {
                    message.blocks.remove(index);
                }
            }
        }
    }
}

fn media_type(bytes: &[u8]) -> Option<&'static str> {
    match bytes {
        [0x89, b'P', b'N', b'G', ..] => Some("image/png"),
        [0xFF, 0xD8, 0xFF, ..] => Some("image/jpeg"),
        [b'G', b'I', b'F', b'8', ..] => Some("image/gif"),
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => Some("image/webp"),
        _ => None,
    }
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for (i, shift) in [18, 12, 6, 0].into_iter().enumerate() {
            if i <= chunk.len() {
                out.push(char::from(ALPHABET[((n >> shift) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{attach_tool_images, base64, drop_old_tool_images, image_size, TOOL_IMAGE_MARKER};
    use crate::session::{ContentBlock, ConversationMessage};

    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 13];
        bytes.extend_from_slice(b"IHDR");
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes
    }

    #[test]
    fn images_over_the_results_budget_are_named_instead_of_attached() {
        let dir = std::env::temp_dir().join(format!("claw-image-budget-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let paths = (0..20)
            .map(|i| {
                let path = dir.join(format!("page{i}.png"));
                std::fs::write(&path, png(1500, 1000)).unwrap();
                path.display().to_string()
            })
            .collect::<Vec<_>>();
        assert_eq!(image_size(&png(1500, 1000)), Some((1500, 1000)));
        let output = paths
            .iter()
            .map(|path| format!("{TOOL_IMAGE_MARKER}{path}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut message = ConversationMessage::tool_result("t", "pdf", output, false);
        attach_tool_images(&mut message);
        // 25k tokens of budget, 1,600 per page: 15 attached, 5 named.
        assert_eq!(images(&message), 15);
        let ContentBlock::ToolResult { output, .. } = &message.blocks[0] else {
            panic!("tool result first");
        };
        assert!(output.contains("5 more image(s) not attached"), "{output}");
        let _ = std::fs::remove_dir_all(dir);
    }

    fn images(message: &ConversationMessage) -> usize {
        message
            .blocks
            .iter()
            .filter(|block| matches!(block, ContentBlock::Image { .. }))
            .count()
    }

    #[test]
    fn named_images_are_attached_and_old_ones_dropped() {
        let dir = std::env::temp_dir().join(format!("claw-tool-images-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("shot.png");
        std::fs::write(&png, [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]).unwrap();
        let output = format!(
            "{{\"ok\":true}}\n{TOOL_IMAGE_MARKER}{}\n{TOOL_IMAGE_MARKER}/missing.png",
            png.display()
        );
        let mut messages: Vec<ConversationMessage> = (0..3)
            .map(|i| {
                let mut message =
                    ConversationMessage::tool_result(format!("t{i}"), "X11", output.clone(), false);
                attach_tool_images(&mut message);
                message
            })
            .collect();
        assert_eq!(images(&messages[0]), 1);
        drop_old_tool_images(&mut messages, 2);
        assert_eq!(
            messages.iter().map(images).collect::<Vec<_>>(),
            vec![0, 1, 1]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn base64_matches_the_standard_alphabet() {
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"fo"), "Zm8=");
    }
}
