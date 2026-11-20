use super::{inspect, ProjectChecks};
use crate::tests::RuntimeFixture;
use aris_fs::{FsService, PathGuard, PathGuardConfig};
use aris_tools::{ToolContext, ToolHandler};
use serde_json::{json, Value};
use std::{fs, path::Path, sync::Arc};

fn fixture() -> (RuntimeFixture, ProjectChecks) {
    let fixture = RuntimeFixture::new(Default::default(), false);
    let runtime = &fixture.runtime;
    let guard = PathGuard::new(PathGuardConfig {
        workspace_root: runtime.workspace_root.clone(),
        outer_home: runtime.outer_home.clone(),
        workspace_outer: runtime.workspace_outer.clone(),
        deny_core_writes: true,
        deny_patterns: vec![],
    })
    .unwrap();
    (fixture, ProjectChecks(Arc::new(FsService::new(guard))))
}

fn write(root: &Path, path: &str, text: &str) {
    let path = root.join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

#[tokio::test]
async fn nested_package_uses_parent_pnpm_lock_without_running_scripts() {
    let (fixture, tool) = fixture();
    let runtime = &fixture.runtime;
    let root = &runtime.workspace_root;
    let manifest = json!({"scripts": {
        "test:unit": "echo ran > aris-script-ran",
        "pretest:unit": "echo hook > aris-hook-ran",
        "start": "serve"
    }})
    .to_string();
    write(root, "pnpm-lock.yaml", "lockfileVersion: '9.0'\n");
    write(root, "apps/shop/package.json", &manifest);
    write(root, "apps/shop/src/main.ts", "");
    let ctx = ToolContext {
        agent_id: "test".into(),
        cwd: root.clone(),
        outer_home: runtime.outer_home.clone(),
        workspace_outer: runtime.workspace_outer.clone(),
        cancel: tokio::sync::watch::channel(false).1,
    };
    let result = tool
        .call(json!({"path": "apps/shop/src/main.ts"}), &ctx)
        .await
        .unwrap();
    let result: Value = serde_json::from_str(&result).unwrap();
    let locations = result["locations"].as_array().unwrap();
    assert_eq!(locations[0]["directory"], "apps/shop");
    assert_eq!(locations[0]["declared_check_scripts"], json!(["test:unit"]));
    assert_eq!(locations[0]["checks"].as_array().unwrap().len(), 1);
    let check = &locations[0]["checks"][0];
    assert_eq!(check["argv"], json!(["pnpm", "run", "test:unit"]));
    assert_eq!(check["source"], "apps/shop/package.json");
    assert_eq!(check["manager_source"], "pnpm-lock.yaml");
    assert_eq!(locations[1]["directory"], ".");
    assert!(!result.to_string().contains("echo"));
    assert_eq!(
        fs::read(root.join("apps/shop/package.json")).unwrap(),
        manifest.as_bytes()
    );
    for dir in [root.clone(), root.join("apps/shop")] {
        assert!(!dir.join("aris-script-ran").exists());
        assert!(!dir.join("aris-hook-ran").exists());
        assert!(!dir.join("node_modules").exists());
    }
}

#[test]
fn detects_pytest_configuration_and_ignores_unrelated_python_projects() {
    let (fixture, tool) = fixture();
    let root = &fixture.runtime.workspace_root;
    for (dir, name, contents) in [
        ("ini", "pytest.ini", "[pytest]\n"),
        (
            "pyproject",
            "pyproject.toml",
            "[tool.pytest.ini_options]\naddopts = '-q'\n",
        ),
        ("tox", "tox.ini", "[pytest]\n"),
        ("cfg", "setup.cfg", "[tool:pytest]\n"),
    ] {
        let source = format!("{dir}/{name}");
        write(root, &source, contents);
        let result = inspect(&tool.0, &root.join(dir)).unwrap();
        let check = &result["locations"][0]["checks"][0];
        assert_eq!(check["argv"], json!(["python", "-m", "pytest"]), "{source}");
        assert_eq!(check["source"], source);
    }
    write(
        root,
        "docs/pyproject.toml",
        "# [tool.pytest]\n[tool.black]\n",
    );
    let result = inspect(&tool.0, &root.join("docs")).unwrap();
    assert_eq!(result["locations"][0]["checks"], json!([]));
}

#[test]
fn malformed_manifests_report_issues_while_retaining_valid_parent_checks() {
    let (fixture, tool) = fixture();
    let root = &fixture.runtime.workspace_root;
    write(root, "pytest.ini", "[pytest]\n");
    write(root, "vendor/package.json", "{\"scripts\":");
    write(root, "vendor/pyproject.toml", "[tool.pytest\n");
    let result = inspect(&tool.0, &root.join("vendor")).unwrap();
    let issue_text = result["locations"][0]["issues"].to_string();
    assert!(issue_text.contains("package.json"), "{issue_text}");
    assert!(issue_text.contains("pyproject.toml"), "{issue_text}");
    assert_eq!(result["locations"][0]["checks"], json!([]));
    assert_eq!(result["locations"][1]["checks"][0]["source"], "pytest.ini");
}

#[test]
fn invalid_utf8_stays_local_to_its_manifest() {
    let (fixture, tool) = fixture();
    let root = &fixture.runtime.workspace_root;
    write(root, "pytest.ini", "[pytest]\n");
    write(
        root,
        "service/pyproject.toml",
        "[tool.pytest.ini_options]\n",
    );
    fs::write(root.join("service/package.json"), [b'{', 0xff, b'}']).unwrap();

    let result = inspect(&tool.0, &root.join("service")).unwrap();
    let locations = result["locations"].as_array().unwrap();
    assert!(locations[0]["issues"].to_string().contains("package.json"));
    assert_eq!(
        locations[0]["checks"][0]["source"],
        "service/pyproject.toml"
    );
    assert_eq!(locations[1]["checks"][0]["source"], "pytest.ini");
}

#[test]
fn rejects_sibling_paths_even_when_the_directory_name_starts_with_workspace() {
    let (fixture, tool) = fixture();
    let root = &fixture.runtime.workspace_root;
    let parent = root.parent().unwrap();
    write(
        parent,
        "workspace-other/package.json",
        r#"{"scripts":{"test":"echo outside"}}"#,
    );
    let escaped = parent.join("workspace-other/package.json");
    assert!(inspect(&tool.0, &escaped).is_err());
    assert!(inspect(&tool.0, &root.join("../workspace-other")).is_err());
}
