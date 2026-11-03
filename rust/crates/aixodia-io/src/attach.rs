//! Pictures that ride on the message that asked about them.
//!
//! Port of the Go `io/gateway/attach.go`.

use std::path::{Path, PathBuf};

use aixodia_provider::{Image, Message};

use crate::gateway::Gateway;

/// One file the phone uploaded before sending its message.
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct ChatAttachment {
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub mime: String,
}

/// What one attached picture may cost in a request body. It is deliberately
/// larger than the tool-result ceiling: a picture sent as an image part is
/// billed as image tokens, not as the characters its base64 would take, so the
/// cheap path is also the one that can afford detail.
pub const IMAGE_BUDGET: usize = 1500 * 1024;

/// Caps a single turn. Every picture costs tokens and time, and a message with a
/// dozen of them is a mistake more often than an intent.
pub const MAX_IMAGES_PER_MESSAGE: usize = 8;

impl Gateway {
    /// Reads the pictures among the attachments and hangs them on the last user
    /// message, which is the one being asked.
    ///
    /// Only files inside the upload directory are opened. The path arrives from
    /// the phone, and a path is a request to read a file: without this check the
    /// model could be handed `/etc/shadow` by a message that says so.
    pub(crate) fn attach_images(
        &self,
        messages: &[Message],
        items: &[ChatAttachment],
    ) -> (Vec<Message>, Vec<String>) {
        let upload_dir = self.upload_dir();
        if items.is_empty() || upload_dir.is_empty() {
            return (messages.to_vec(), Vec::new());
        }
        let target = messages.iter().rposition(|message| message.role == "user");
        let Some(target) = target else {
            return (messages.to_vec(), Vec::new());
        };

        let mut images: Vec<Image> = Vec::new();
        let mut skipped: Vec<String> = Vec::new();
        for item in items {
            if images.len() >= MAX_IMAGES_PER_MESSAGE {
                skipped.push("too many pictures in one message".to_string());
                break;
            }
            let path = match self.resolve_upload(&item.path) {
                Ok(path) => path,
                Err(err) => {
                    skipped.push(format!("{}: {err}", name_or(item)));
                    continue;
                }
            };
            let raw = match std::fs::read(&path) {
                Ok(raw) => raw,
                Err(_) => {
                    skipped.push(format!("{}: cannot read it", name_or(item)));
                    continue;
                }
            };
            // Not a picture, or not one this can decode. The text note already
            // covers it, so nothing is reported: a PDF is not a failure.
            let Ok(pic) = aixodia_imageprep::prepare_within(&raw, IMAGE_BUDGET) else {
                continue;
            };
            images.push(Image {
                media_type: pic.media_type,
                data: pic.data,
            });
        }
        if images.is_empty() {
            return (messages.to_vec(), skipped);
        }

        // Copy rather than mutate: the caller still holds the original slice and
        // uses it to decide what the stored turn says.
        let mut out = messages.to_vec();
        out[target].images.extend(images);
        (out, skipped)
    }

    /// Turns a reported path into a real one, and refuses anything outside the
    /// upload directory.
    pub(crate) fn resolve_upload(&self, path: &str) -> anyhow::Result<PathBuf> {
        let path = path.trim();
        if path.is_empty() {
            anyhow::bail!("no path");
        }
        let raw = Path::new(path);
        if !raw.is_absolute() {
            anyhow::bail!("path must be absolute");
        }
        let upload_dir = self.upload_dir();
        let root = std::path::absolute(Path::new(upload_dir.trim()))
            .map_err(|_| anyhow::anyhow!("upload directory is unusable"))?;
        let clean = std::path::absolute(raw).map_err(|_| anyhow::anyhow!("path is unusable"))?;
        // Resolve symlinks so a symlink inside the upload directory cannot point
        // out of it. The file has to exist for this to work, which is also
        // required.
        let resolved =
            std::fs::canonicalize(&clean).map_err(|_| anyhow::anyhow!("file not found"))?;
        if resolved != root && !resolved.starts_with(&root) {
            anyhow::bail!("outside the upload directory");
        }
        Ok(resolved)
    }
}

fn name_or(item: &ChatAttachment) -> String {
    if !item.name.trim().is_empty() {
        return item.name.clone();
    }
    match Path::new(&item.path).file_name().and_then(|n| n.to_str()) {
        Some(base) if !base.is_empty() && base != "." => base.to_string(),
        _ => "ไฟล์".to_string(),
    }
}
