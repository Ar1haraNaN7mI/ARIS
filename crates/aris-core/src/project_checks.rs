//! Read the workspace manifests around a source file. This tool describes
//! possible checks; only a separate shell receipt can establish their outcome.
use anyhow::{ensure, Context, Result};
use aris_fs::FsService;
use aris_tools::{ToolContext, ToolDefinition, ToolError, ToolHandler, ToolMetadata};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::Path;
use std::sync::Arc;

pub(crate) struct ProjectChecks(pub Arc<FsService>);

const FILES: &[&str] = &[
    "AGENTS.md",
    "CONTRIBUTING.md",
    "README.md",
    "README.rst",
    "README",
    "package.json",
    "package-lock.json",
    "npm-shrinkwrap.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "bun.lock",
    "bun.lockb",
    "Cargo.toml",
    "go.mod",
    "pyproject.toml",
    "pytest.ini",
    "tox.ini",
    "setup.cfg",
    "Makefile",
];

fn verification_script(name: &str) -> bool {
    name.bytes()
        .all(|c| c.is_ascii_alphanumeric() || b":-_.".contains(&c))
        && matches!(
            name.split(':').next(),
            Some("test" | "check" | "build" | "lint" | "typecheck")
        )
}

/// Search this directory's metadata only. Never source scripts or invoke a
/// package manager to discover a workspace.
fn manifest_text(
    fs: &FsService,
    dir: &Path,
    name: &str,
    issues: &mut Vec<String>,
) -> Option<String> {
    match fs.read_text(dir.join(name)) {
        Ok(text) => Some(text.trim_start_matches('\u{feff}').to_owned()),
        Err(error) => {
            issues.push(format!("Could not read {name}: {error}"));
            None
        }
    }
}

fn node_checks(
    fs: &FsService,
    dir: &Path,
    files: &[&str],
    relative: &impl Fn(&str) -> String,
    inherited_manager: &mut Option<(String, String)>,
    checks: &mut Vec<Value>,
    issues: &mut Vec<String>,
) -> Vec<String> {
    let mut checked_scripts = Vec::<String>::new();
    let mut declared_manager = None;
    if let Some(text) = files
        .contains(&"package.json")
        .then(|| manifest_text(fs, dir, "package.json", issues))
        .flatten()
    {
        match serde_json::from_str::<Value>(&text) {
            Ok(pkg) if pkg.is_object() => {
                if let Some(manager) = pkg["packageManager"]
                    .as_str()
                    .and_then(|v| v.split('@').next())
                {
                    if matches!(manager, "npm" | "pnpm" | "yarn" | "bun") {
                        declared_manager = Some((manager.to_owned(), relative("package.json")));
                    } else {
                        issues.push("Unrecognized packageManager in package.json".to_string());
                    }
                }
                if let Some(scripts) = pkg["scripts"].as_object() {
                    checked_scripts.extend(
                        scripts
                            .iter()
                            .filter(|(name, body)| verification_script(name) && body.is_string())
                            .map(|(name, _)| name.clone()),
                    );
                }
            }
            Ok(_) => issues.push("package.json must contain an object".into()),
            Err(error) => issues.push(format!("Invalid package.json: {error}")),
        }
    }
    let locks = [
        ("pnpm", "pnpm-lock.yaml"),
        ("yarn", "yarn.lock"),
        ("npm", "npm-shrinkwrap.json"),
        ("npm", "package-lock.json"),
        ("bun", "bun.lock"),
        ("bun", "bun.lockb"),
    ];
    let mut managers = std::collections::BTreeMap::new();
    for (manager, file) in locks {
        if files.contains(&file) {
            managers.entry(manager).or_insert(file);
        }
    }
    if declared_manager.is_some() {
        *inherited_manager = declared_manager;
    } else if managers.len() == 1 && issues.is_empty() {
        let (&name, &file) = managers.first_key_value().unwrap();
        *inherited_manager = Some((name.to_string(), relative(file)));
    } else if managers.len() > 1 || !issues.is_empty() {
        *inherited_manager = None;
        if managers.len() > 1 {
            issues.push(
                "Conflicting package-manager lockfiles; check the repository instructions".into(),
            );
        }
    }
    for name in &checked_scripts {
        if let Some((manager, source)) = inherited_manager.as_ref() {
            checks.push(json!({"argv":[manager,"run",name],"source":relative("package.json"),"manager_source":source}));
        }
    }
    checked_scripts
}

fn pytest_source(
    fs: &FsService,
    dir: &Path,
    files: &[&str],
    issues: &mut Vec<String>,
) -> Option<&'static str> {
    let mut pytest_config = files.contains(&"pytest.ini").then_some("pytest.ini");
    if pytest_config.is_none() && files.contains(&"pyproject.toml") {
        if let Some(text) = manifest_text(fs, dir, "pyproject.toml", issues) {
            match text.parse::<toml::Value>() {
                Ok(doc) if doc.get("tool").and_then(|t| t.get("pytest")).is_some() => {
                    pytest_config = Some("pyproject.toml")
                }
                Ok(_) => {}
                Err(error) => issues.push(format!(
                    "Invalid pyproject.toml at {}",
                    error
                        .span()
                        .map_or_else(|| "unknown position".into(), |p| p.start.to_string())
                )),
            }
        }
    }
    if pytest_config.is_none() {
        for (file, section) in [("setup.cfg", "[tool:pytest]"), ("tox.ini", "[pytest]")] {
            if files.contains(&file)
                && manifest_text(fs, dir, file, issues)
                    .is_some_and(|text| text.lines().any(|l| l.trim() == section))
            {
                pytest_config = Some(file);
                break;
            }
        }
    }
    pytest_config
}

fn inspect(fs: &FsService, target: &Path) -> Result<Value> {
    let root = fs.guard().config().workspace_root.canonicalize()?;
    let resolved = target
        .canonicalize()
        .context("Select an existing workspace file or directory")?;
    ensure!(
        resolved.starts_with(&root),
        "Select a path inside the current workspace"
    );
    let leaf = if resolved.is_file() {
        resolved.parent().context("File has no parent")?
    } else {
        &resolved
    };
    ensure!(leaf.is_dir(), "Expected a source file or directory");
    let mut dirs: Vec<_> = leaf
        .ancestors()
        .take_while(|p| p.starts_with(&root))
        .collect();
    dirs.reverse();
    let mut projects = Vec::new();
    let mut inherited_manager: Option<(String, String)> = None;
    for dir in dirs {
        let rel = dir
            .strip_prefix(&root)?
            .to_string_lossy()
            .replace('\\', "/");
        let cwd = if rel.is_empty() { "." } else { &rel };
        let relative = |name: &str| {
            if rel.is_empty() {
                name.to_string()
            } else {
                format!("{rel}/{name}")
            }
        };
        let mut files = Vec::new();
        for &name in FILES {
            let file = dir.join(name);
            if file.is_file() && file.canonicalize()?.starts_with(&root) {
                fs.guard().check_read(&file)?;
                files.push(name);
            }
        }
        if files.is_empty() {
            continue;
        }
        let mut checks = Vec::new();
        let mut issues = Vec::new();
        let checked_scripts = node_checks(
            fs,
            dir,
            &files,
            &relative,
            &mut inherited_manager,
            &mut checks,
            &mut issues,
        );
        for (file, argv) in [
            ("Cargo.toml", &["cargo", "test"][..]),
            ("go.mod", &["go", "test", "./..."][..]),
        ] {
            if files.contains(&file) {
                checks.push(json!({"argv":argv,"source":relative(file)}));
            }
        }
        if let Some(source) = pytest_source(fs, dir, &files, &mut issues) {
            checks.push(json!({"argv":["python","-m","pytest"],"source":relative(source)}));
        }
        projects.push(json!({
            "directory": cwd,
            "files": files.into_iter().map(relative).collect::<Vec<_>>(),
            "declared_check_scripts": checked_scripts,
            "checks": checks,
            "issues": issues,
        }));
    }
    projects.reverse();
    Ok(json!({"locations":projects}))
}

#[async_trait]
impl ToolHandler for ProjectChecks {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::builtin("project_checks","Find local build and test entry points, lockfiles and contribution guides for a file or package, nearest first. Each check lists its working directory (location.directory), argument vector and source. Discovery does not execute a check.",
            json!({"type":"object","properties":{"path":{"type":"string","description":"Existing source file or package directory; defaults to the workspace."}}}))
            .with_metadata(ToolMetadata::read_only())
    }

    async fn call(&self, args: Value, ctx: &ToolContext) -> Result<String, ToolError> {
        if *ctx.cancel.borrow() {
            return Err(ToolError::Message("cancelled".into()));
        }
        let path = ctx.cwd.join(args["path"].as_str().unwrap_or("."));
        inspect(&self.0, &path)
            .map(|v| v.to_string())
            .map_err(|e| ToolError::Message(e.to_string()))
    }
}

#[cfg(test)]
mod tests;
