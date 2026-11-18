//! Outer-layer credentials (API keys). Never stored under the core crate tree.

use anyhow::{Context, Result};
use aris_llm::LlmBackend;
use std::fs;
use std::path::{Path, PathBuf};

const CREDENTIALS_FILE: &str = "credentials.env";

pub fn credentials_path(outer_home: &Path) -> PathBuf {
    outer_home.join(CREDENTIALS_FILE)
}

/// Resolve an LLM API key from (in order):
/// 1. process env `DEEPSEEK_API_KEY`, `ARIS_LLM_API_KEY`, `OPENAI_API_KEY`
/// 2. outer `credentials.env`
/// 3. empty string (boot still succeeds; configure later)
pub fn resolve_api_key(outer_home: &Path) -> String {
    resolve_api_key_with_variables(
        outer_home,
        &[
            "KEI_API_KEY",
            "ARIS_API_KEY",
            "DEEPSEEK_API_KEY",
            "ARIS_LLM_API_KEY",
            "OPENAI_API_KEY",
        ],
    )
}

/// Resolve credentials with a backend-aware precedence. In particular, a
/// globally configured `OPENAI_API_KEY` must not accidentally be sent to the
/// hosted DeepSeek endpoint when `DEEPSEEK_API_KEY` is absent.
pub fn resolve_api_key_for_backend(outer_home: &Path, backend: LlmBackend) -> String {
    let variables: &[&str] = if backend == LlmBackend::Kei {
        &["KEI_API_KEY", "ARIS_API_KEY", "ARIS_LLM_API_KEY"]
    } else if backend == LlmBackend::DeepSeek {
        &["DEEPSEEK_API_KEY", "ARIS_LLM_API_KEY", "OPENAI_API_KEY"]
    } else {
        &[
            "ARIS_API_KEY",
            "KEI_API_KEY",
            "ARIS_LLM_API_KEY",
            "OPENAI_API_KEY",
            "DEEPSEEK_API_KEY",
        ]
    };
    resolve_api_key_with_variables(outer_home, &variables)
}

fn resolve_api_key_with_variables(outer_home: &Path, variables: &[&str]) -> String {
    for variable in variables {
        if let Ok(key) = std::env::var(variable) {
            let key = key.trim().to_string();
            if !key.is_empty() {
                return key;
            }
        }
    }
    load_api_key(outer_home).unwrap_or_default()
}

pub fn load_api_key(outer_home: &Path) -> Option<String> {
    let path = credentials_path(outer_home);
    let text = fs::read_to_string(path).ok()?;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        for variable in [
            "KEI_API_KEY",
            "ARIS_API_KEY",
            "ARIS_LLM_API_KEY",
            "OPENAI_API_KEY",
            "DEEPSEEK_API_KEY",
        ] {
            if let Some(rest) = line.strip_prefix(&format!("{variable}=")) {
                let v = rest.trim().trim_matches('"').trim().to_string();
                if !v.is_empty() {
                    return Some(v);
                }
            }
        }
    }
    None
}

pub fn load_named_api_key(outer_home: &Path, name: &str) -> Option<String> {
    if !name.starts_with("ARIS_KEY_")
        || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return None;
    }
    fs::read_to_string(outer_home.join("credentials.d").join(name))
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

pub fn save_named_api_key(outer_home: &Path, name: &str, key: &str) -> Result<()> {
    anyhow::ensure!(
        name.starts_with("ARIS_KEY_")
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
        "invalid credential reference"
    );
    anyhow::ensure!(
        !key.trim().is_empty() && !key.chars().any(char::is_control),
        "invalid API key"
    );
    let dir = outer_home.join("credentials.d");
    fs::create_dir_all(&dir)?;
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    use std::io::Write;
    let mut file = options.open(dir.join(name))?;
    file.write_all(key.trim().as_bytes())?;
    file.sync_all()?;
    Ok(())
}

pub fn save_api_key(outer_home: &Path, api_key: &str) -> Result<PathBuf> {
    let key = api_key.trim();
    anyhow::ensure!(
        !key.is_empty() && key.len() <= 8192 && !key.chars().any(char::is_control),
        "API key must contain 1..8192 printable characters"
    );
    fs::create_dir_all(outer_home).with_context(|| format!("create {}", outer_home.display()))?;
    let path = credentials_path(outer_home);
    let body = format!("# ARIS outer credentials (do not commit)\nKEI_API_KEY={key}\n");
    let temp = outer_home.join(format!(".credentials-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> Result<()> {
        use std::io::Write;
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temp)?;
        file.write_all(body.as_bytes())?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temp, &path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result.with_context(|| format!("write {}", path.display()))?;
    // Keep process env in sync for this session.
    std::env::set_var("DEEPSEEK_API_KEY", key);
    std::env::set_var("ARIS_LLM_API_KEY", key);
    std::env::set_var("KEI_API_KEY", key);
    std::env::set_var("ARIS_API_KEY", key);
    Ok(path)
}

pub fn clear_api_key(outer_home: &Path) -> Result<()> {
    let path = credentials_path(outer_home);
    if path.exists() {
        fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
    }
    std::env::remove_var("DEEPSEEK_API_KEY");
    std::env::remove_var("ARIS_LLM_API_KEY");
    std::env::remove_var("KEI_API_KEY");
    std::env::remove_var("ARIS_API_KEY");
    Ok(())
}

pub fn api_key_status(outer_home: &Path) -> String {
    let from_env = [
        "KEI_API_KEY",
        "ARIS_API_KEY",
        "DEEPSEEK_API_KEY",
        "ARIS_LLM_API_KEY",
        "OPENAI_API_KEY",
    ]
    .into_iter()
    .find_map(|variable| {
        std::env::var(variable)
            .ok()
            .filter(|s| !s.trim().is_empty())
            .map(|value| (variable, value))
    });
    let from_file = load_api_key(outer_home);
    match (from_env, from_file) {
        (Some((variable, k)), _) => format!("configured ({variable}, …{})", mask_tail(&k)),
        (None, Some(k)) => format!(
            "configured ({} , …{})",
            credentials_path(outer_home).display(),
            mask_tail(&k)
        ),
        (None, None) => {
            "not configured — run `aris config set-api-key <KEY>` or TUI `/apikey <KEY>`".into()
        }
    }
}

fn mask_tail(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    if chars.len() <= 4 {
        "****".into()
    } else {
        chars[chars.len().saturating_sub(4)..].iter().collect()
    }
}
