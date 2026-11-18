//! Workspace-scoped media tools. Provider configuration is operator-owned and
//! captured at startup; a model may choose content, never a credential endpoint.
use anyhow::{ensure, Context, Result};
use aris_fs::{FsService, PathGuard, PathGuardConfig};
use aris_tools::{
    ToolCapability, ToolContext, ToolDefinition, ToolError, ToolHandler, ToolMetadata, ToolRegistry,
};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    path::{Component, Path, PathBuf},
    process::Stdio,
    sync::Arc,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::Command,
};

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct MediaSettings {
    image: ImageSettings,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ImageSettings {
    enabled: bool,
    model: String,
    protocol: ImageProtocol,
    image_model: Option<String>,
    api_key_env: Option<String>,
    response_format: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, serde::Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
enum ImageProtocol {
    #[default]
    Images,
    Responses,
}

#[derive(Clone)]
struct Provider {
    image: ImageSettings,
    base_url: String,
    key_name: String,
    default_key: bool,
}

fn provider_key(provider: &Provider, outer_home: &Path) -> Option<String> {
    std::env::var(&provider.key_name)
        .ok()
        .filter(|k| !k.trim().is_empty())
        .or_else(|| crate::credentials::load_named_api_key(outer_home, &provider.key_name))
        .or_else(|| {
            if provider.default_key {
                std::env::var("ARIS_API_KEY")
                    .ok()
                    .filter(|k| !k.trim().is_empty())
            } else {
                None
            }
        })
        .or_else(|| {
            if provider.default_key {
                crate::credentials::load_api_key(outer_home)
            } else {
                None
            }
        })
}

fn provider(outer_home: &Path, fallback: &crate::KeiConfig) -> Result<Provider> {
    let path = outer_home.join("media.json");
    let settings: MediaSettings = if path.is_file() {
        serde_json::from_slice(&std::fs::read(path)?).context("Invalid outer media.json")?
    } else {
        MediaSettings::default()
    };
    let image = settings.image;
    ensure!(
        image.model.len() <= 256 && !image.model.chars().any(char::is_control),
        "Invalid image model ID"
    );
    if let Some(model) = &image.image_model {
        ensure!(
            !model.trim().is_empty() && model.len() <= 256 && !model.chars().any(char::is_control),
            "Invalid Responses image tool model ID"
        );
        ensure!(
            image.protocol == ImageProtocol::Responses,
            "image_model is only used with the responses protocol"
        );
    }
    ensure!(
        image
            .response_format
            .as_deref()
            .is_none_or(|v| v == "b64_json"),
        "Image response_format must be omitted or b64_json"
    );
    let gateway = crate::kei::load_kei_config(outer_home, fallback)?;
    let base_url = crate::kei::api_root(&gateway.resolved_base_url()).to_owned();
    crate::kei::validate_gateway_url(&base_url)?;
    let parsed = reqwest::Url::parse(&base_url)?;
    ensure!(
        parsed.scheme() == "https"
            || matches!(parsed.host_str(), Some("localhost" | "127.0.0.1" | "[::1]")),
        "Image API requires HTTPS except for loopback fixtures"
    );
    let key_name = image.api_key_env.clone().unwrap_or(gateway.api_key_env);
    ensure!(
        !key_name.is_empty()
            && key_name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_'),
        "Invalid image credential reference"
    );
    let default_key = key_name == "KEI_API_KEY";
    Ok(Provider {
        image,
        base_url,
        key_name,
        default_key,
    })
}

pub fn register(
    registry: &ToolRegistry,
    config: &crate::AppConfig,
    workspace: &Path,
    outer_home: &Path,
) -> Result<()> {
    let fs = Arc::new(FsService::new(PathGuard::new(PathGuardConfig {
        workspace_root: workspace.to_owned(),
        outer_home: outer_home.to_owned(),
        workspace_outer: workspace.join(&config.paths.workspace_outer),
        deny_core_writes: config.guard.deny_core_writes,
        deny_patterns: config.guard.deny_patterns.clone(),
    })?));
    let provider = provider(outer_home, &config.kei).map_err(|e| e.to_string());
    for operation in [
        "media_status",
        "media_probe",
        "video_edit",
        "poster_render",
        "image_generate",
    ] {
        registry.register(Arc::new(MediaTool {
            operation,
            fs: fs.clone(),
            provider: provider.clone(),
        }));
    }
    Ok(())
}

struct MediaTool {
    operation: &'static str,
    fs: Arc<FsService>,
    provider: std::result::Result<Provider, String>,
}

fn path_schema() -> Value {
    json!({"type":"string","minLength":1,"maxLength":4096})
}
fn schema(operation: &str) -> Value {
    let path = path_schema();
    let color = json!({"type":"string","minLength":7,"maxLength":9});
    let element = |kind: &str, extra: Value, required: Vec<&str>| {
        let mut fields = json!({"type":{"type":"string","enum":[kind]},"x":{"type":"integer","minimum":0,"maximum":4095},"y":{"type":"integer","minimum":0,"maximum":4095}});
        fields
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let mut names = vec!["type", "x", "y"];
        names.extend(required);
        json!({"type":"object","properties":fields,"required":names,"additionalProperties":false})
    };
    let properties = match operation {
        "media_status" => json!({}),
        "media_probe" => json!({"path":path}),
        "video_edit" => json!({
            "clips":{"type":"array","minItems":1,"maxItems":12,"items":{"type":"object","properties":{
                "path":path,"start_seconds":{"type":"number","minimum":0,"maximum":86400},"duration_seconds":{"type":"number","minimum":0.05,"maximum":600}},
                "required":["path","duration_seconds"],"additionalProperties":false}},
            "output":path,"width":{"type":"integer","minimum":64,"maximum":3840},"height":{"type":"integer","minimum":64,"maximum":2160},
            "fps":{"type":"integer","minimum":1,"maximum":60},"audio":{"type":"string","enum":["preserve","mute"]}}),
        "poster_render" => {
            json!({"output":path,"width":{"type":"integer","minimum":64,"maximum":4096},
            "height":{"type":"integer","minimum":64,"maximum":4096},"background":color,
            "elements":{"type":"array","maxItems":80,"items":{"oneOf":[
                element("text",json!({"text":{"type":"string","minLength":1,"maxLength":2000},"font":path,"font_size":{"type":"integer","minimum":8,"maximum":512},"color":color}),vec!["text"]),
                element("rectangle",json!({"width":{"type":"integer","minimum":1,"maximum":4096},"height":{"type":"integer","minimum":1,"maximum":4096},"color":color}),vec!["width","height"]),
                element("image",json!({"width":{"type":"integer","minimum":1,"maximum":4096},"height":{"type":"integer","minimum":1,"maximum":4096},"path":path}),vec!["width","height","path"])
            ]}}})
        }
        "image_generate" => {
            json!({"prompt":{"type":"string","minLength":1,"maxLength":16000},"output":path,
            "size":{"type":"string","minLength":1,"maxLength":64},"quality":{"type":"string","minLength":1,"maxLength":64}})
        }
        _ => unreachable!(),
    };
    let required = match operation {
        "media_status" => vec![],
        "media_probe" => vec!["path"],
        "video_edit" => vec!["clips", "output"],
        "poster_render" => vec!["output", "width", "height", "elements"],
        _ => vec!["prompt", "output"],
    };
    json!({"type":"object","properties":properties,"required":required,"additionalProperties":false})
}

fn runtime_path(ctx: &ToolContext) -> Option<PathBuf> {
    let mut candidates = vec![];
    if let Some(path) = std::env::var_os("ARIS_MEDIA_RUNTIME") {
        candidates.push(PathBuf::from(path));
    }
    candidates.push(ctx.outer_home.join("media/media.py"));
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent().and_then(Path::parent) {
            candidates.push(parent.join("share/aris/media/media.py"));
        }
    }
    candidates
        .push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../integrations/media/media.py"));
    candidates.into_iter().find(|p| p.is_file())
}

fn checked_path(fs: &FsService, ctx: &ToolContext, value: &str, write: bool) -> Result<()> {
    let p = Path::new(value);
    ensure!(
        !value.chars().any(char::is_control)
            && !p.components().any(|c| matches!(c, Component::ParentDir)),
        "Media paths may not traverse parent directories"
    );
    let path = if p.is_absolute() {
        p.to_path_buf()
    } else {
        ctx.cwd.join(p)
    };
    let root = ctx.cwd.canonicalize()?;
    let mut ancestor = path.as_path();
    while !ancestor.exists() {
        ancestor = ancestor.parent().context("Invalid media path")?;
    }
    ensure!(
        ancestor.canonicalize()?.starts_with(&root),
        "Media files must stay inside the workspace"
    );
    if write {
        fs.guard().check_write(&path)?;
        ensure!(
            !path.exists(),
            "Output already exists; choose a new filename"
        );
    } else {
        fs.guard().check_read(&path)?;
        ensure!(path.is_file(), "Input media file does not exist");
    }
    Ok(())
}

#[async_trait]
impl ToolHandler for MediaTool {
    fn definition(&self) -> ToolDefinition {
        let description = match self.operation {
            "media_status" => "Check local FFmpeg/ffprobe/Pillow and configured image-generation model availability. Host subscriptions and host imagegen are not inherited.",
            "media_probe" => "Inspect a local workspace video or PNG/JPEG/WebP image; returns verified dimensions, duration and codecs. No remote URLs or desktop capture.",
            "video_edit" => "Cut and concatenate 1-12 local workspace video clips, max total 600 seconds, into a new verified H264 MP4. Defaults 1280x720, 30fps; preserve audio pads silent clips, or choose mute. Files are never overwritten. Requires local FFmpeg/ffprobe.",
            "poster_render" => "Render an offline poster to a new PNG using declared text, rectangles and local images. Coordinates are pixels; text must fit. CJK text auto-selects an installed Microsoft YaHei/PingFang/Noto CJK font; an explicit font path must be a workspace file. Returns actual font names. Requires Pillow. This is layout rendering, not AI image generation.",
            _ => "Generate an image through the operator-configured KEI Images API or Responses image_generation tool, then save verified PNG in the workspace. May incur provider charges. Never accepts endpoint, model IDs or credentials from arguments. Requires outer media.json with exact authorized model and a base64-image result; unavailable capability returns an explicit error.",
        };
        let mut metadata = if matches!(self.operation, "media_status" | "media_probe") {
            ToolMetadata::read_only()
        } else {
            ToolMetadata::write()
        };
        metadata.timeout_secs = Some(if self.operation == "video_edit" {
            300
        } else if self.operation == "image_generate" {
            360
        } else {
            35
        });
        if !matches!(self.operation, "media_status" | "media_probe") {
            metadata.idempotent = false;
            metadata.capabilities.push(ToolCapability::Read);
            if self.operation == "image_generate" {
                metadata
                    .capabilities
                    .extend([ToolCapability::Network, ToolCapability::Secret]);
            } else {
                metadata.capabilities.push(ToolCapability::Process);
            }
        }
        let mut definition =
            ToolDefinition::builtin(self.operation, description, schema(self.operation))
                .with_metadata(metadata);
        definition.tags.push("media".into());
        definition
    }

    async fn call(&self, args: Value, ctx: &ToolContext) -> Result<String, ToolError> {
        aris_tools::validate_tool_arguments(&self.definition(), &args)?;
        if *ctx.cancel.borrow() {
            return Err(ToolError::Message("Media operation cancelled".into()));
        }
        for name in ["path", "output"] {
            if let Some(value) = args.get(name).and_then(Value::as_str) {
                checked_path(&self.fs, ctx, value, name == "output")?;
            }
        }
        for array in ["clips", "elements"] {
            for item in args
                .get(array)
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                for name in ["path", "font"] {
                    if let Some(value) = item.get(name).and_then(Value::as_str) {
                        checked_path(&self.fs, ctx, value, false)?;
                    }
                }
            }
        }
        let mut envelope = json!({"operation":self.operation,"args":args,"workspace":ctx.cwd});
        match &self.provider {
            Ok(provider) => {
                envelope["provider"] = json!({"enabled":provider.image.enabled,"model":provider.image.model,"protocol":provider.image.protocol,"image_model":provider.image.image_model});
                if self.operation == "media_status" {
                    envelope["provider"]["credential_configured"] =
                        json!(provider_key(provider, &ctx.outer_home).is_some());
                }
                if self.operation == "image_generate" {
                    if !provider.image.enabled || provider.image.model.trim().is_empty() {
                        return Err(ToolError::Message("Image generation is not configured; set an enabled exact model in outer media.json and restart ARIS".into()));
                    }
                    let key = provider_key(provider, &ctx.outer_home)
                        .context("Image provider credential is missing; configure its operator-selected credential reference")?;
                    envelope["provider"]["api_key"] = json!(key);
                    envelope["provider"]["base_url"] = json!(provider.base_url);
                    envelope["provider"]["response_format"] = json!(provider.image.response_format);
                }
            }
            Err(error) if self.operation == "image_generate" => {
                return Err(ToolError::Message(format!(
                    "Image configuration is invalid: {error}"
                )))
            }
            Err(error) => {
                envelope["provider"] =
                    json!({"enabled":false,"model":null,"configuration_error":error});
            }
        }
        let path = runtime_path(ctx).context("Media adapter is not installed; run scripts/install_aris.py or configure ARIS_MEDIA_RUNTIME")?;
        let venv = path.parent().unwrap().join(if cfg!(windows) {
            ".venv/Scripts/python.exe"
        } else {
            ".venv/bin/python"
        });
        let python = std::env::var_os("ARIS_MEDIA_PYTHON")
            .or_else(|| venv.is_file().then(|| venv.into_os_string()))
            .unwrap_or_else(|| "python".into());
        let mut command = Command::new(python);
        command
            .args(["-X", "utf8"])
            .arg(path)
            .current_dir(&ctx.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(windows)]
        command.creation_flags(0x08000000);
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command.spawn().context(
            "Could not start the media adapter; check Python and the media requirements",
        )?;
        let pid = child.id();
        let mut stdin = child.stdin.take().context("Media stdin unavailable")?;
        stdin
            .write_all(envelope.to_string().as_bytes())
            .await
            .map_err(anyhow::Error::from)?;
        drop(stdin);
        let stdout = tokio::spawn(read_bounded(
            child.stdout.take().context("Media stdout unavailable")?,
            128 * 1024,
        ));
        let stderr = tokio::spawn(read_bounded(
            child.stderr.take().context("Media stderr unavailable")?,
            64 * 1024,
        ));
        let mut cancel = ctx.cancel.clone();
        // Finish process-tree cleanup before the outer tool deadline drops this future.
        let deadline = self
            .definition()
            .metadata
            .timeout_secs
            .unwrap()
            .saturating_sub(5);
        let status = tokio::select! {
            status = child.wait() => status.map_err(anyhow::Error::from)?,
            _ = wait_cancel(&mut cancel) => { stop_tree(pid).await; let _ = child.kill().await; stdout.abort(); stderr.abort(); return Err(ToolError::Message("Media operation cancelled".into())); },
            _ = tokio::time::sleep(std::time::Duration::from_secs(deadline)) => { stop_tree(pid).await; let _ = child.kill().await; stdout.abort(); stderr.abort(); return Err(ToolError::Message("Media operation exceeded its deadline".into())); },
        };
        let output = stdout.await.map_err(anyhow::Error::from)??;
        let _ = stderr.await.map_err(anyhow::Error::from)??;
        if !status.success() {
            return Err(ToolError::Message(
                "Media adapter failed; check its local dependencies".into(),
            ));
        }
        let value: Value =
            serde_json::from_slice(&output).context("Invalid media adapter response")?;
        if let Some(error) = value.get("error") {
            return Err(ToolError::Message(format!(
                "Media {}: {}",
                error["code"].as_str().unwrap_or("failed"),
                error["message"].as_str().unwrap_or("Operation failed")
            )));
        }
        Ok(value.to_string())
    }
}

async fn read_bounded(reader: impl AsyncRead + Unpin, limit: usize) -> Result<Vec<u8>> {
    let mut bytes = vec![];
    reader
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .await?;
    ensure!(
        bytes.len() <= limit,
        "Media adapter output exceeded its limit"
    );
    Ok(bytes)
}
async fn wait_cancel(cancel: &mut tokio::sync::watch::Receiver<bool>) {
    if *cancel.borrow() {
        return;
    }
    while cancel.changed().await.is_ok() {
        if *cancel.borrow() {
            return;
        }
    }
    std::future::pending::<()>().await;
}
async fn stop_tree(pid: Option<u32>) {
    let Some(pid) = pid else {
        return;
    };
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .creation_flags(0x08000000)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
    }
    #[cfg(unix)]
    {
        let _ = Command::new("kill")
            .args(["-TERM", "--", &format!("-{pid}")])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn schema_rejects_endpoint_injection_and_unbounded_edits() {
        let definition = |op| ToolDefinition::builtin(op, "test", schema(op));
        assert!(aris_tools::validate_tool_arguments(
            &definition("image_generate"),
            &json!({"prompt":"test","output":"a.png","base_url":"https://attacker.test"})
        )
        .is_err());
        assert!(aris_tools::validate_tool_arguments(
            &definition("video_edit"),
            &json!({"clips":[{"path":"a.mp4","duration_seconds":601}],"output":"b.mp4"})
        )
        .is_err());
        assert!(aris_tools::validate_tool_arguments(&definition("poster_render"), &json!({"width":100,"height":100,"output":"p.png","elements":[{"type":"text","x":0,"y":0,"text":"ok"}]})).is_ok());
    }
    #[test]
    fn media_paths_respect_guard_and_workspace_boundaries() {
        let root = std::env::temp_dir().join(format!("aris-media-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("crates")).unwrap();
        let fs = FsService::new(
            PathGuard::new(PathGuardConfig {
                workspace_root: root.clone(),
                outer_home: root.join(".outer"),
                workspace_outer: root.join(".aris"),
                deny_core_writes: true,
                deny_patterns: vec![],
            })
            .unwrap(),
        );
        let (_, cancel) = tokio::sync::watch::channel(false);
        let ctx = ToolContext {
            agent_id: "test".into(),
            cwd: root.clone(),
            outer_home: root.join(".outer"),
            workspace_outer: root.join(".aris"),
            cancel,
        };
        assert!(checked_path(&fs, &ctx, "poster.png", true).is_ok());
        assert!(checked_path(&fs, &ctx, "../escape.png", true).is_err());
        assert!(checked_path(&fs, &ctx, "crates/overwrite.png", true).is_err());
        std::fs::write(root.join("poster.png"), b"existing").unwrap();
        assert!(checked_path(&fs, &ctx, "poster.png", true).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn configured_image_model_is_explicit_and_media_writes_follow_permissions() {
        let root = std::env::temp_dir().join(format!("aris-media-config-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let default = provider(&root, &crate::KeiConfig::default()).unwrap();
        assert!(!default.image.enabled);
        std::fs::write(
            root.join("media.json"),
            r#"{"image":{"enabled":true,"model":"operator-model","api_key_env":"IMAGE_TEST_KEY"}}"#,
        )
        .unwrap();
        let selected = provider(&root, &crate::KeiConfig::default()).unwrap();
        assert_eq!(selected.image.model, "operator-model");
        assert_eq!(selected.image.protocol, ImageProtocol::Images);
        assert!(!selected.default_key);
        let fs = Arc::new(FsService::new(
            PathGuard::new(PathGuardConfig {
                workspace_root: root.clone(),
                outer_home: root.clone(),
                workspace_outer: root.join(".aris"),
                deny_core_writes: true,
                deny_patterns: vec![],
            })
            .unwrap(),
        ));
        for operation in ["video_edit", "poster_render", "image_generate"] {
            let tool = MediaTool {
                operation,
                fs: fs.clone(),
                provider: Ok(selected.clone()),
            };
            let metadata = tool.definition().metadata;
            assert!(metadata.requires_approval);
            assert!(!crate::PermissionMode::ReadOnly.allows_metadata(&metadata));
        }
        std::fs::write(root.join("media.json"), r#"{"image":{"enabled":true,"model":"operator-model","base_url":"https://untrusted.test"}}"#).unwrap();
        assert!(provider(&root, &crate::KeiConfig::default()).is_err());
        std::fs::write(root.join("media.json"), r#"{"image":{"enabled":true,"protocol":"responses","model":"operator-responses-model","image_model":"operator-image-model"}}"#).unwrap();
        let responses = provider(&root, &crate::KeiConfig::default()).unwrap();
        assert_eq!(responses.image.protocol, ImageProtocol::Responses);
        assert_eq!(
            responses.image.image_model.as_deref(),
            Some("operator-image-model")
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    #[ignore = "Requires Python + installed media Pillow; run explicitly for media integration validation"]
    async fn real_adapter_creates_verified_png_and_observes_cancellation() {
        let root =
            std::env::temp_dir().join(format!("aris-media-adapter-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let fs = Arc::new(FsService::new(
            PathGuard::new(PathGuardConfig {
                workspace_root: root.clone(),
                outer_home: root.join(".outer"),
                workspace_outer: root.join(".aris"),
                deny_core_writes: true,
                deny_patterns: vec![],
            })
            .unwrap(),
        ));
        let (cancel_tx, cancel) = tokio::sync::watch::channel(false);
        let ctx = ToolContext {
            agent_id: "integration".into(),
            cwd: root.clone(),
            outer_home: root.join(".outer"),
            workspace_outer: root.join(".aris"),
            cancel,
        };
        let tool = MediaTool {
            operation: "poster_render",
            fs,
            provider: Err("Image provider is intentionally not configured".into()),
        };
        let args = json!({"output":"deliverables/poster.png","width":300,"height":400,"elements":[{"type":"text","x":20,"y":30,"text":"ARIS","font_size":32}]});
        let value: Value = serde_json::from_str(&tool.call(args, &ctx).await.unwrap()).unwrap();
        assert_eq!(value["artifacts"][0]["width"], 300);
        assert_eq!(value["generated_by_model"], false);
        let png = std::fs::read(root.join("deliverables/poster.png")).unwrap();
        assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
        cancel_tx.send(true).unwrap();
        assert!(tool
            .call(
                json!({"output":"cancelled.png","width":100,"height":100,"elements":[]}),
                &ctx
            )
            .await
            .unwrap_err()
            .to_string()
            .contains("cancelled"));
        assert!(!root.join("cancelled.png").exists());
        std::fs::remove_dir_all(root).unwrap();
    }
}
