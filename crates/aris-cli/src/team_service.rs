use anyhow::{Context, Result};
use aris_core::{
    harness::TeamSpec,
    kei::{load_kei_config, save_kei_config, KeiConfig},
    Runtime,
};
use serde_json::{json, Value};
use std::sync::Arc;

fn string<'a>(value: &'a Value, name: &str) -> Result<&'a str> {
    value
        .get(name)
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .with_context(|| format!("{name} is required"))
}

pub async fn dispatch(
    runtime: Arc<Runtime>,
    method: &str,
    params: &Value,
) -> Option<Result<Value>> {
    if !method.starts_with("teams/")
        && !method.starts_with("kei/")
        && !method.starts_with("templates/")
    {
        return None;
    }
    Some(handle(runtime, method, params).await)
}

async fn handle(runtime: Arc<Runtime>, method: &str, params: &Value) -> Result<Value> {
    Ok(match method {
        "kei/import/start" => crate::kei_import::start(&runtime)?,
        "kei/import/status" => crate::kei_import::status(&runtime),
        "templates/list" => json!({"templates":aris_core::templates::list(&runtime.outer_home)?}),
        "templates/save" => {
            json!({"template":aris_core::templates::save(&runtime.outer_home,serde_json::from_value(params.get("template").cloned().context("template is required")?)?)?})
        }
        "templates/delete" => {
            aris_core::templates::delete(&runtime.outer_home, string(params, "id")?)?;
            json!({"deleted":true})
        }
        "teams/list" => json!({"teams":runtime.teams.list()?}),
        "teams/get" => json!({"team":runtime.teams.get(string(params,"id")?)?}),
        "teams/plan" => json!({"team":crate::team_planner::plan(&runtime,params).await?}),
        "teams/admin" => {
            json!({"accepted":true,"team":crate::team_planner::admin(runtime.clone(),params)?})
        }
        "teams/model" => json!({"team":runtime.teams.set_model(
            &runtime, string(params,"id")?, string(params,"agent_id")?, string(params,"profile_id")?
        )?}),
        "teams/create" => {
            let spec: TeamSpec = serde_json::from_value(params.clone())?;
            json!({"team":runtime.teams.create(&runtime,spec)?})
        }
        "teams/run" => {
            json!({"accepted":true,"team":runtime.teams.start_all(runtime.clone(),string(params,"id")?)?})
        }
        "teams/cancel" => json!({"team":runtime.teams.cancel(string(params,"id")?)?}),
        "teams/message" => {
            let id = string(params, "id")?;
            let agent = match params.get("agent_id") {
                None | Some(Value::Null) => None,
                Some(Value::String(value)) if !value.is_empty() => Some(value.as_str()),
                _ => anyhow::bail!("agent_id must be a nonempty string or null"),
            };
            let team = if params.get("cascade").and_then(Value::as_bool) == Some(true) {
                runtime
                    .teams
                    .message(id, agent, string(params, "message")?)?
            } else {
                runtime
                    .teams
                    .chat_message(id, agent, string(params, "message")?)?
            };
            if matches!(team.status.as_str(), "running" | "cancelling") {
                json!({"accepted":true,"team":team})
            } else {
                // A just-completed run may still own its lock for a few milliseconds.
                let mut retries = 0;
                let team = loop {
                    match runtime.teams.start(runtime.clone(), id) {
                        Ok(team) => break team,
                        Err(error) if error.to_string().contains("already running") => {
                            let current = runtime.teams.get(id)?;
                            if matches!(current.status.as_str(), "running" | "cancelling") {
                                break current;
                            }
                            if retries >= 10 {
                                return Err(error);
                            }
                            retries += 1;
                            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
                        }
                        Err(error) => return Err(error),
                    }
                };
                json!({"accepted":true,"team":team})
            }
        }
        "teams/checkpoint" => {
            json!({"team":runtime.teams.checkpoint(string(params,"id")?,string(params,"label")?)?})
        }
        "teams/restore" => {
            json!({"team":runtime.teams.restore(string(params,"id")?,string(params,"checkpoint_id")?)?})
        }
        "teams/handoff" => {
            let id = string(params, "id")?;
            runtime.teams.handoff(
                id,
                string(params, "from")?,
                string(params, "to")?,
                params
                    .get("instruction")
                    .and_then(Value::as_str)
                    .unwrap_or("Continue from the previous agent's deliverable"),
            )?;
            json!({"accepted":true,"team":runtime.teams.start(runtime.clone(),id)?})
        }
        "kei/config" => kei_status(&runtime)?,
        "kei/update" => {
            let mut config = load_kei_config(&runtime.outer_home, &runtime.config.kei)?;
            if let Some(url) = params.get("base_url") {
                config.base_url = url
                    .as_str()
                    .context("base_url must be a string")?
                    .to_owned();
            }
            if let Some(profiles) = params.get("profiles") {
                config.profiles = serde_json::from_value(profiles.clone())?;
            }
            save_kei_config(&runtime.outer_home, &config)?;
            if let Some(profile) = config
                .profiles
                .iter()
                .find(|p| p.id == "gpt" && !p.model.is_empty())
                .or_else(|| config.profiles.iter().find(|p| !p.model.is_empty()))
            {
                if let Ok(resolved) =
                    profile.resolve(&config, &runtime.llm.config(), &runtime.outer_home)
                {
                    runtime.llm.replace_config(resolved);
                }
            }
            kei_status(&runtime)?
        }
        "kei/credential" => {
            aris_core::save_api_key(&runtime.outer_home, string(params, "api_key")?)?;
            let mut config = load_kei_config(&runtime.outer_home, &runtime.config.kei)?;
            config.api_key_env = "KEI_API_KEY".into();
            save_kei_config(&runtime.outer_home, &config)?;
            kei_status(&runtime)?
        }
        "kei/models" => {
            let config = load_kei_config(&runtime.outer_home, &runtime.config.kei)?;
            let key = kei_key(&runtime, &config)?;
            let base = config.base_url.trim_end_matches('/');
            let url = if base.ends_with("/v1") {
                format!("{base}/models")
            } else {
                format!("{base}/v1/models")
            };
            let response = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .redirect(reqwest::redirect::Policy::none())
                .build()?
                .get(url)
                .bearer_auth(key)
                .send()
                .await?;
            anyhow::ensure!(
                response.status().is_success(),
                "KEI model discovery returned HTTP {}",
                response.status()
            );
            let body: Value = response.json().await?;
            let models = body
                .get("data")
                .and_then(Value::as_array)
                .context("KEI /models returned no data array")?;
            json!({"models":models.iter().filter_map(|m|m.get("id").and_then(Value::as_str).map(|id|json!({"id":id,"owned_by":m.get("owned_by")}))).collect::<Vec<_>>()})
        }
        _ => anyhow::bail!("unknown ARIS method: {method}"),
    })
}
fn kei_key(runtime: &Runtime, config: &KeiConfig) -> Result<String> {
    std::env::var(&config.api_key_env)
        .ok()
        .filter(|k| !k.trim().is_empty())
        .or_else(|| {
            aris_core::credentials::load_named_api_key(&runtime.outer_home, &config.api_key_env)
        })
        .or_else(|| {
            std::env::var("ARIS_API_KEY")
                .ok()
                .filter(|k| !k.trim().is_empty())
        })
        .or_else(|| aris_core::load_api_key(&runtime.outer_home))
        .context("Set KEI_API_KEY or save an API key in ARIS model settings")
}
pub fn kei_status(runtime: &Runtime) -> Result<Value> {
    let config = load_kei_config(&runtime.outer_home, &runtime.config.kei)?;
    Ok(
        json!({"base_url":config.base_url,"profiles":config.profiles,"configured":kei_key(runtime,&config).is_ok()}),
    )
}
