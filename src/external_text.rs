use std::path::Path;

/// Plain text (Telegram HTML formatting allowed, no templating) loaded fresh from disk at
/// startup, so it can be edited directly without needing a Rust rebuild - just restart the
/// container (e.g. via `./reload-env.sh`) to pick up changes.
#[derive(Clone)]
pub struct ExternalTexts {
    pub syntax: String,
    pub intro: String,
}

impl ExternalTexts {
    pub fn load(dir: &str) -> std::io::Result<Self> {
        Ok(Self {
            syntax: std::fs::read_to_string(Path::new(dir).join("syntax.html"))?,
            intro: std::fs::read_to_string(Path::new(dir).join("intro.html"))?,
        })
    }
}
