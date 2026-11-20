use serde_json::{json, Value};
use std::{fs, path::Path, process::Output, time::Duration};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

async fn cli(workspace: &Path, endpoint: &str, args: &[&str]) -> Output {
    tokio::time::timeout(
        Duration::from_secs(45),
        tokio::process::Command::new(env!("CARGO_BIN_EXE_aris"))
            .current_dir(workspace)
            .env("ARIS_HOME", workspace.join("home"))
            .env("KEI_BASE_URL", endpoint)
            .env("KEI_API_KEY", "loopback-only")
            .env("ARIS_LLM_BACKEND", "kei")
            .env("ARIS_LLM_FALLBACK_BASE_URLS", "")
            .env("ARIS_MODEL", "local-fixture")
            .env("NO_PROXY", "127.0.0.1")
            .args(args)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .unwrap()
    .unwrap()
}

#[tokio::test]
async fn cli_preserves_stream_final_message_and_native_exit_status_across_attempts() {
    let workspace =
        std::env::temp_dir().join(format!("aris-cli-acceptance-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(workspace.join(".aris")).unwrap();
    let mut config: toml::Value = include_str!("../../../config/default.toml")
        .parse()
        .unwrap();
    config["learn"]["enabled"] = false.into();
    fs::write(
        workspace.join(".aris/config.toml"),
        toml::to_string(&config).unwrap(),
    )
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
    let (started, live) = tokio::sync::oneshot::channel();
    let (release, wait) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let mut pause = Some((started, wait));
        for text in [
            "Premature pass.",
            "Still unverified.",
            "File is missing.",
            "File is ready.",
            "Ordinary answer.",
        ] {
            let (socket, _) = listener.accept().await.unwrap();
            let mut io = BufReader::new(socket);
            let mut length = None;
            loop {
                let mut line = String::new();
                assert_ne!(io.read_line(&mut line).await.unwrap(), 0);
                if line == "\r\n" {
                    break;
                }
                if let Some(size) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = Some(size.trim().parse::<usize>().unwrap());
                }
            }
            io.read_exact(&mut vec![0; length.unwrap()]).await.unwrap();
            if text == "File is ready." {
                let (started, wait) = pause.take().unwrap();
                started.send(()).unwrap();
                wait.await.unwrap();
            }
            let delta = json!({"choices":[{"index":0,"delta":{"role":"assistant","content":text},"finish_reason":"stop"}]});
            let body = format!("data: {delta}\n\ndata: [DONE]\n\n");
            let response = format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len());
            io.get_mut().write_all(response.as_bytes()).await.unwrap();
        }
    });
    let first = cli(
        &workspace,
        &endpoint,
        &[
            "exec",
            "--json",
            "--last-message-file",
            "last.txt",
            "Fix it and run `cargo test`.",
        ],
    )
    .await;
    let stderr = String::from_utf8_lossy(&first.stderr);
    assert!(
        !first.status.success() && stderr.contains("No matching command"),
        "{stderr}"
    );
    let events: Vec<Value> = std::str::from_utf8(&first.stdout)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(events.first().unwrap()["event_type"], "agent.turn_started");
    assert_eq!(events.last().unwrap()["event_type"], "agent.done");
    assert!(events.iter().any(|e| e["event_type"] == "agent.error"));
    let deltas: String = events
        .iter()
        .filter(|e| e["event_type"] == "agent.text_delta")
        .map(|e| e["payload"]["text"].as_str().unwrap())
        .collect();
    assert_eq!(deltas, "Premature pass.Still unverified.");
    let last = workspace.join("last.txt");
    assert_eq!(fs::read_to_string(&last).unwrap(), "Still unverified.");

    let task = events[0]["task_id"].as_str().unwrap();
    let contract = cli(
        &workspace,
        &endpoint,
        &["task", "verify", task, "file_exists:approved.txt"],
    )
    .await;
    assert!(
        contract.status.success(),
        "{}",
        String::from_utf8_lossy(&contract.stderr)
    );
    let resume = [
        "task",
        "resume",
        task,
        "--prompt",
        "Inspect the delivery.",
        "--last-message-file",
        "last.txt",
    ];
    let missing = cli(&workspace, &endpoint, &resume).await;
    let stderr = String::from_utf8_lossy(&missing.stderr);
    assert!(
        !missing.status.success() && stderr.contains("file_exists:approved.txt"),
        "{stderr}"
    );
    assert_eq!(fs::read_to_string(&last).unwrap(), "File is missing.");
    fs::write(workspace.join("approved.txt"), "ready").unwrap();
    let (complete, ()) = tokio::join!(cli(&workspace, &endpoint, &resume), async {
        tokio::time::timeout(Duration::from_secs(15), live)
            .await
            .unwrap()
            .unwrap();
        let duplicate = cli(&workspace, &endpoint, &resume).await;
        let stderr = String::from_utf8_lossy(&duplicate.stderr);
        assert!(
            !duplicate.status.success() && stderr.contains("already active"),
            "{stderr}"
        );
        release.send(()).unwrap();
    });
    assert!(
        complete.status.success(),
        "{}",
        String::from_utf8_lossy(&complete.stderr)
    );
    assert_eq!(fs::read_to_string(&last).unwrap(), "File is ready.");
    let ordinary = cli(
        &workspace,
        &endpoint,
        &["exec", "--last-message-file", "last.txt", "Say hello."],
    )
    .await;
    assert!(
        ordinary.status.success(),
        "{}",
        String::from_utf8_lossy(&ordinary.stderr)
    );
    assert_eq!(fs::read_to_string(&last).unwrap(), "Ordinary answer.");
    server.await.unwrap();
    config["agent"]["max_steps_per_turn"] = 0.into();
    fs::write(
        workspace.join(".aris/config.toml"),
        toml::to_string(&config).unwrap(),
    )
    .unwrap();
    let interrupted = cli(
        &workspace,
        &endpoint,
        &[
            "exec-resume",
            "--last",
            "--last-message-file",
            "last.txt",
            "Follow up.",
        ],
    )
    .await;
    let stderr = String::from_utf8_lossy(&interrupted.stderr);
    assert!(
        !interrupted.status.success() && stderr.contains("no executable steps"),
        "{stderr}"
    );
    assert_eq!(fs::read_to_string(&last).unwrap(), "");
    fs::remove_dir_all(workspace).unwrap();
}
