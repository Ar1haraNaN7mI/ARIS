//! Provider profiles sharing the operator's KEI gateway and credentials.
//! A family is a display/routing label; it never invents a provider model ID.
use anyhow::{bail, ensure, Context, Result};
use aris_llm::{LlmBackend, LlmConfig, LlmProtocol};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::HashSet, path::Path};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelFamily {
    Deepseek,
    Grok,
    Gemini,
    Gpt,
    Glm,
    Kimi,
}

impl ModelFamily {
    pub const ALL: [Self; 6] = [
        Self::Deepseek,
        Self::Grok,
        Self::Gemini,
        Self::Gpt,
        Self::Glm,
        Self::Kimi,
    ];
    pub fn id(self) -> &'static str {
        match self {
            Self::Deepseek => "deepseek",
            Self::Grok => "grok",
            Self::Gemini => "gemini",
            Self::Gpt => "gpt",
            Self::Glm => "glm",
            Self::Kimi => "kimi",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelProfileConfig {
    pub id: String,
    #[serde(default)]
    pub name: String,
    pub family: ModelFamily,
    /// Exact upstream identifier returned by KEI /models or supplied by the operator.
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub protocol: LlmProtocol,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub api_key_env: Option<String>,
    #[serde(default)]
    pub thinking: Option<bool>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub extra_body: Option<Value>,
    /// Role guidance is private to this agent's turn, never a global prompt mutation.
    #[serde(default)]
    pub instructions: String,
    #[serde(default)]
    pub vision: bool,
}

impl ModelProfileConfig {
    pub fn new(family: ModelFamily, model: impl Into<String>) -> Self {
        Self {
            id: family.id().into(),
            name: family.id().into(),
            family,
            model: model.into(),
            protocol: LlmProtocol::ChatCompletions,
            base_url: None,
            api_key_env: None,
            thinking: None,
            max_tokens: None,
            temperature: None,
            extra_body: None,
            instructions: String::new(),
            vision: false,
        }
    }

    pub fn resolve(
        &self,
        gateway: &KeiConfig,
        defaults: &LlmConfig,
        outer_home: &Path,
    ) -> Result<LlmConfig> {
        self.validate()?;
        let base_url = match &self.base_url {
            Some(value) => value.trim().to_string(),
            None => gateway.resolved_base_url(),
        };
        validate_gateway_url(&base_url)?;
        let model = if self.model.trim().is_empty() {
            nonempty_env(&format!(
                "KEI_MODEL_{}",
                self.family.id().to_ascii_uppercase()
            ))
            .unwrap_or_default()
        } else {
            self.model.trim().to_string()
        };
        ensure!(!model.is_empty(), "Profile '{}' needs a real KEI model ID; configure kei.profiles[].model or KEI_MODEL_{} (see aris models)", self.id, self.family.id().to_ascii_uppercase());
        let key_env = self.api_key_env.as_deref().unwrap_or(&gateway.api_key_env);
        let api_key = nonempty_env(key_env)
            .or_else(|| crate::credentials::load_named_api_key(outer_home, key_env))
            .or_else(|| {
                if key_env == "KEI_API_KEY" {
                    nonempty_env("ARIS_API_KEY")
                } else {
                    None
                }
            })
            .or_else(|| {
                if self.api_key_env.is_none() {
                    crate::credentials::load_api_key(outer_home)
                } else {
                    None
                }
            })
            .unwrap_or_default();
        ensure!(
            !api_key.is_empty(),
            "KEI credentials are missing; set {key_env} or run aris config set-api-key <KEY>"
        );
        Ok(LlmConfig {
            api_key,
            base_url,
            model,
            backend: LlmBackend::Kei,
            protocol: self.protocol,
            thinking: self.thinking.unwrap_or(defaults.thinking),
            max_tokens: self.max_tokens.unwrap_or(defaults.max_tokens),
            temperature: self.temperature.unwrap_or(defaults.temperature),
            extra_body: self.extra_body.clone(),
            fallbacks: Vec::new(),
        })
    }

    fn validate(&self) -> Result<()> {
        ensure!(
            !self.id.trim().is_empty()
                && self.id.len() <= 128
                && self
                    .id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_".contains(c)),
            "Profile id must contain 1..128 letters, digits, '-' or '_'"
        );
        ensure!(
            !self.model.chars().any(char::is_control),
            "Model ID contains control characters"
        );
        if let Some(url) = &self.base_url {
            validate_gateway_url(url)?;
        }
        if let Some(name) = &self.api_key_env {
            validate_env_name(name)?;
        }
        if let Some(value) = self.max_tokens {
            ensure!(
                value > 0 && value <= 2_000_000,
                "max_tokens must be between 1 and 2000000"
            );
        }
        if let Some(value) = self.temperature {
            ensure!(
                value.is_finite() && (0.0..=2.0).contains(&value),
                "temperature must be between 0 and 2"
            );
        }
        if let Some(value) = &self.extra_body {
            ensure!(value.is_object(), "extra_body must be a JSON object");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct KeiConfig {
    pub base_url: String,
    pub api_key_env: String,
    pub profiles: Vec<ModelProfileConfig>,
}

impl Default for KeiConfig {
    fn default() -> Self {
        Self {
            base_url: "https://kei-code.com/v1".into(),
            api_key_env: "KEI_API_KEY".into(),
            profiles: ModelFamily::ALL
                .into_iter()
                .map(|f| ModelProfileConfig::new(f, ""))
                .collect(),
        }
    }
}

impl KeiConfig {
    pub fn profile(&self, id: &str) -> Result<ModelProfileConfig> {
        let id = if id == "codex" { "gpt" } else { id };
        self.profiles
            .iter()
            .find(|p| p.id == id)
            .cloned()
            .with_context(|| format!("Unknown model profile '{id}'; configure a KEI profile first"))
    }

    pub fn resolved_base_url(&self) -> String {
        nonempty_env("KEI_BASE_URL")
            .or_else(|| nonempty_env("ARIS_BASE_URL"))
            .unwrap_or_else(|| self.base_url.trim().to_string())
    }

    pub fn validate(&self) -> Result<()> {
        if !self.base_url.is_empty() {
            validate_gateway_url(&self.base_url)?;
        }
        validate_env_name(&self.api_key_env)?;
        ensure!(
            self.profiles.len() <= 128,
            "At most 128 model profiles are supported"
        );
        let mut ids = HashSet::new();
        for profile in &self.profiles {
            profile.validate()?;
            ensure!(
                ids.insert(&profile.id),
                "Duplicate profile id '{}'",
                profile.id
            );
        }
        Ok(())
    }

    /// Read the gateway's actual catalog; no vendor aliases or model availability are assumed.
    pub async fn discover_models(&self, outer_home: &Path) -> Result<Vec<String>> {
        let base = self.resolved_base_url();
        validate_gateway_url(&base)?;
        let key = nonempty_env(&self.api_key_env)
            .or_else(|| crate::credentials::load_named_api_key(outer_home, &self.api_key_env))
            .or_else(|| crate::credentials::load_api_key(outer_home))
            .context("KEI credentials missing; set KEI_API_KEY")?;
        let response = reqwest::Client::new()
            .get(format!("{}/models", api_root(&base)))
            .timeout(std::time::Duration::from_secs(25))
            .bearer_auth(key)
            .send()
            .await
            .context("KEI /models request failed")?;
        ensure!(
            response.status().is_success(),
            "KEI /models returned {}",
            response.status()
        );
        let value: Value = response
            .json()
            .await
            .context("KEI /models returned invalid JSON")?;
        let entries = value
            .get("data")
            .and_then(Value::as_array)
            .context("KEI /models response must contain a data array")?;
        let mut ids: Vec<String> = entries
            .iter()
            .filter_map(|v| v.get("id").and_then(Value::as_str))
            .map(str::to_owned)
            .collect();
        ids.sort();
        ids.dedup();
        Ok(ids)
    }
}

pub fn api_root(base: &str) -> &str {
    let base = base.trim_end_matches('/');
    base.strip_suffix("/chat/completions")
        .or_else(|| base.strip_suffix("/responses"))
        .unwrap_or(base)
}

fn nonempty_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
}

fn validate_env_name(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
        "api_key_env must be an environment variable name"
    );
    Ok(())
}

pub fn validate_gateway_url(base: &str) -> Result<()> {
    if base.trim().is_empty() {
        bail!("KEI gateway URL is not configured; set KEI_BASE_URL to your gateway's API base (including /v1 when required)");
    }
    let url = reqwest::Url::parse(base).context("Invalid KEI gateway URL")?;
    ensure!(
        matches!(url.scheme(), "http" | "https")
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "KEI URL must be HTTP(S) without credentials, query or fragment"
    );
    Ok(())
}

pub fn load_kei_config(outer_home: &Path, fallback: &KeiConfig) -> Result<KeiConfig> {
    let path = outer_home.join("kei.json");
    let config = if path.exists() {
        serde_json::from_slice(&std::fs::read(&path)?)?
    } else {
        fallback.clone()
    };
    config.validate()?;
    Ok(config)
}

pub fn save_kei_config(outer_home: &Path, config: &KeiConfig) -> Result<()> {
    config.validate()?;
    std::fs::create_dir_all(outer_home)?;
    let temporary = outer_home.join(format!(".kei-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> Result<()> {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(&serde_json::to_vec_pretty(config)?)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temporary, outer_home.join("kei.json"))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn defaults_never_invent_gateway_or_upstream_ids() {
        let config = KeiConfig::default();
        assert_eq!(config.base_url, "https://kei-code.com/v1");
        assert_eq!(config.profiles.len(), 6);
        assert!(config.profiles.iter().all(|p| p.model.is_empty()));
        assert_eq!(config.profile("codex").unwrap().family, ModelFamily::Gpt);
    }
    #[test]
    fn rejects_ambiguous_or_credential_bearing_config() {
        let mut config = KeiConfig::default();
        config.profiles.push(config.profiles[0].clone());
        assert!(config.validate().is_err());
        for url in [
            "",
            "file:///tmp/a",
            "https://key:secret@example.com/v1",
            "https://example.com/v1?key=secret",
        ] {
            assert!(validate_gateway_url(url).is_err());
        }
        assert!(validate_gateway_url("http://localhost:5000/v1").is_ok());
    }
    #[test]
    fn profile_resolves_own_model_and_protocol_without_mutating_defaults() {
        let dir = std::env::temp_dir().join(format!("aris-kei-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("credentials.env"), "KEI_API_KEY=fixture-key\n").unwrap();
        let mut profile = ModelProfileConfig::new(ModelFamily::Gpt, "actual-upstream-id");
        profile.protocol = LlmProtocol::Responses;
        profile.base_url = Some("https://gateway.example/v1".into());
        let defaults = LlmConfig::default();
        let resolved = profile
            .resolve(&KeiConfig::default(), &defaults, &dir)
            .unwrap();
        assert_eq!(resolved.model, "actual-upstream-id");
        assert_eq!(resolved.protocol, LlmProtocol::Responses);
        assert_eq!(resolved.api_key, "fixture-key");
        assert_ne!(defaults.model, resolved.model);
        std::fs::remove_file(dir.join("credentials.env")).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }
}
