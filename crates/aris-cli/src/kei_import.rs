//! Browser-mediated KEI import. Only an expiring authorization code crosses the URL.
use anyhow::{ensure, Context, Result};
use aris_core::{
    kei::{load_kei_config, save_kei_config, ModelFamily, ModelProfileConfig},
    Runtime,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    sync::OnceLock,
    time::{Duration, Instant},
};

struct Pending {
    state: String,
    verifier: String,
    redirect_uri: String,
    origin: String,
    created: Instant,
}
#[derive(Default)]
struct ImportState {
    authority: String,
    pending: Option<Pending>,
    status: String,
    message: String,
    imported_count: usize,
}
fn states() -> &'static Mutex<HashMap<String, ImportState>> {
    static STATES: OnceLock<Mutex<HashMap<String, ImportState>>> = OnceLock::new();
    STATES.get_or_init(|| Mutex::new(HashMap::new()))
}
fn key(runtime: &Runtime) -> String {
    format!(
        "{}|{}",
        runtime.outer_home.display(),
        runtime.workspace_root.display()
    )
}
pub fn attach(runtime: &Runtime, authority: &str) {
    states().lock().entry(key(runtime)).or_default().authority = authority.into();
}

pub fn start(runtime: &Runtime) -> Result<Value> {
    let config = load_kei_config(&runtime.outer_home, &runtime.config.kei)?;
    let base = reqwest::Url::parse(&config.resolved_base_url())?;
    ensure!(
        base.scheme() == "https"
            || (base.scheme() == "http"
                && matches!(base.host_str(), Some("127.0.0.1" | "localhost"))),
        "KEI import requires HTTPS or a local development server"
    );
    let origin = base.origin().ascii_serialization();
    let mut all = states().lock();
    let entry = all
        .get_mut(&key(runtime))
        .context("Open ARIS web/app before importing models")?;
    ensure!(
        entry.status != "pending"
            || entry
                .pending
                .as_ref()
                .is_some_and(|p| p.created.elapsed() > Duration::from_secs(600)),
        "An import is already pending; finish it or wait for it to expire"
    );
    ensure!(
        !entry.authority.is_empty(),
        "ARIS callback listener is unavailable"
    );
    let verifier = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    let state = uuid::Uuid::new_v4().simple().to_string();
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let redirect_uri = format!("http://{}/api/kei/import/callback", entry.authority);
    let mut url = reqwest::Url::parse(&format!("{origin}/integrations/aris"))?;
    url.query_pairs_mut()
        .append_pair("redirect_uri", &redirect_uri)
        .append_pair("state", &state)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256");
    entry.pending = Some(Pending {
        state,
        verifier,
        redirect_uri,
        origin,
        created: Instant::now(),
    });
    entry.status = "pending".into();
    entry.message.clear();
    entry.imported_count = 0;
    Ok(json!({"url":url.as_str()}))
}
pub fn status(runtime: &Runtime) -> Value {
    let mut all = states().lock();
    let entry = all.entry(key(runtime)).or_default();
    if entry
        .pending
        .as_ref()
        .is_some_and(|p| p.created.elapsed() > Duration::from_secs(600))
    {
        entry.pending = None;
        entry.status = "error".into();
        entry.message = "登录导入已超时，请重新发起。".into();
    }
    json!({"status":if entry.status.is_empty(){"idle"}else{&entry.status},"message":entry.message,"imported_count":entry.imported_count})
}

#[derive(Deserialize)]
struct ImportedModel {
    id: String,
    family: ModelFamily,
    protocol: aris_llm::LlmProtocol,
    #[serde(default)]
    vision: bool,
}
#[derive(Deserialize)]
struct ImportBundle {
    base_url: String,
    api_key: String,
    models: Vec<ImportedModel>,
}

pub async fn callback(runtime: &Runtime, raw_path: &str) -> Result<usize> {
    let parsed = reqwest::Url::parse(&format!("http://127.0.0.1{raw_path}"))?;
    let query: HashMap<_, _> = parsed.query_pairs().into_owned().collect();
    let state = query.get("state").context("Missing import state")?;
    let pending = {
        let mut all = states().lock();
        let entry = all
            .get_mut(&key(runtime))
            .context("No active import request")?;
        let request = entry
            .pending
            .as_ref()
            .context("Import request already consumed or expired")?;
        ensure!(
            request.state == *state && request.created.elapsed() <= Duration::from_secs(600),
            "Invalid or expired import state"
        );
        entry.status = "pending".into();
        entry.message = "正在导入模型…".into();
        entry.pending.take().unwrap()
    };
    let result=async {
        ensure!(!query.contains_key("error"),"KEI import was cancelled");
        let code=query.get("code").filter(|v|!v.is_empty()&&v.len()<=512).context("Missing authorization code")?;
        let client=reqwest::Client::builder().timeout(Duration::from_secs(30)).redirect(reqwest::redirect::Policy::none()).build()?;
        let mut response=client.post(format!("{}/api/aris/import/redeem",pending.origin))
            .json(&json!({"code":code,"code_verifier":pending.verifier,"redirect_uri":pending.redirect_uri})).send().await?;
        ensure!(response.status().is_success(),"KEI import redemption returned HTTP {}",response.status());
        let mut bytes=Vec::new();
        while let Some(chunk)=response.chunk().await? { ensure!(bytes.len()+chunk.len()<=1024*1024,"KEI import response exceeds 1 MiB"); bytes.extend_from_slice(&chunk); }
        let raw:Value=serde_json::from_slice(&bytes)?;
        ensure!(raw.get("success")!=Some(&Value::Bool(false)),"KEI rejected the import code; restart import");
        let bundle:ImportBundle=serde_json::from_value(raw.get("data").cloned().unwrap_or(raw))?;
        let returned=reqwest::Url::parse(&bundle.base_url)?;
        ensure!(returned.origin().ascii_serialization()==pending.origin,"Imported gateway origin does not match the selected KEI server");
        ensure!(!bundle.models.is_empty()&&bundle.models.len()<=128,"Import must contain 1–128 models");
        ensure!(!bundle.api_key.trim().is_empty()&&!bundle.api_key.contains(['\n','\r'])&&bundle.api_key.len()<=4096,"Invalid imported API key");
        let mut config=load_kei_config(&runtime.outer_home,&runtime.config.kei)?;
        config.base_url=bundle.base_url.clone();
        let credential_name=format!("ARIS_KEY_{}",uuid::Uuid::new_v4().simple().to_string().to_uppercase());
        config.api_key_env=credential_name.clone();
        let count=bundle.models.len();
        for model in bundle.models {
            ensure!(!model.id.trim().is_empty()&&model.id.len()<=256&&!model.id.chars().any(char::is_control),"Invalid imported model ID");
            let mut profile=ModelProfileConfig::new(model.family,model.id.clone());
            let hash=hex::encode(Sha256::digest(format!("{}|{}",bundle.base_url,model.id).as_bytes()));
            profile.id=format!("kei-{}-{}",model.family.id(),&hash[..16]);
            profile.name=model.id; profile.protocol=model.protocol; profile.vision=model.vision;
            profile.api_key_env=Some(credential_name.clone());
            profile.base_url=Some(bundle.base_url.clone());
            if let Some(alias)=config.profiles.iter_mut().find(|p|p.id==model.family.id()&&p.model.is_empty()) { let id=alias.id.clone(); *alias=profile.clone(); alias.id=id; }
            if let Some(old)=config.profiles.iter_mut().find(|p|p.id==profile.id) { *old=profile; } else {config.profiles.push(profile);}
        }
        config.validate()?;
        aris_core::credentials::save_named_api_key(&runtime.outer_home,&credential_name,&bundle.api_key)?;
        // The config atomically switches the default credential reference;
        // failure before this point leaves all existing key bindings intact.
        save_kei_config(&runtime.outer_home,&config)?;
        if let Some(profile)=config.profiles.iter().find(|p|p.id=="gpt"&&!p.model.is_empty()).or_else(||config.profiles.iter().find(|p|!p.model.is_empty())) {
            let resolved=profile.resolve(&config,&runtime.llm.config(),&runtime.outer_home)?;
            runtime.llm.replace_config(resolved);
        }
        Ok::<_,anyhow::Error>(count)
    }.await;
    let mut all = states().lock();
    let entry = all.entry(key(runtime)).or_default();
    match &result {
        Ok(count) => {
            entry.status = "completed".into();
            entry.imported_count = *count;
            entry.message = format!("已导入 {count} 个模型");
        }
        Err(_) => {
            entry.status = "error".into();
            entry.message = "导入失败，请重新发起导入并检查 KEI 服务状态。".into();
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pkce_uses_url_safe_sha256() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }
}
