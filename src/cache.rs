//! Hugging Face cache locations. We read the cache directly; we never download (local-only).

use std::path::PathBuf;

/// The HF hub cache directory, matching the precedence `ptts` uses for cache-only resolution.
pub fn hf_hub() -> PathBuf {
    if let Ok(dir) = std::env::var("HF_HUB_CACHE") {
        return PathBuf::from(dir);
    }
    let home = std::env::var("HOME").unwrap_or_default();
    if let Ok(dir) = std::env::var("HF_HOME") {
        return PathBuf::from(dir).join("hub");
    }
    if let Ok(dir) = std::env::var("XDG_CACHE_HOME") {
        return PathBuf::from(dir).join("huggingface").join("hub");
    }
    PathBuf::from(home)
        .join(".cache")
        .join("huggingface")
        .join("hub")
}

/// A repo id such as `gradium/phonon` becomes its cache folder name.
pub fn repo_dir(hub: &std::path::Path, repo: &str) -> PathBuf {
    hub.join(format!("models--{}", repo.replace('/', "--")))
}
