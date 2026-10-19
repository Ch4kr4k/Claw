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
        message.blocks.push(ContentBlock::Image {
            media_type: media_type.to_string(),
            data: base64(&bytes),
            source: path,
        });
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
    use super::{attach_tool_images, base64, drop_old_tool_images, TOOL_IMAGE_MARKER};
    use crate::session::{ContentBlock, ConversationMessage};

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
