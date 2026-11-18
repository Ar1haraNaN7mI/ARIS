//! Isolated browser observation and an explicit operator JSON session.
use anyhow::Result;
use aris_core::Runtime;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::watch;

pub(crate) async fn run(runtime: Arc<Runtime>, action: crate::ComputerCmd) -> Result<()> {
    let (cancel, rx) = watch::channel(false);
    let signal = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            let _ = cancel.send(true);
        }
    });
    let result = match action {
        crate::ComputerCmd::Status => Ok(runtime.computer_status()),
        crate::ComputerCmd::Enable => runtime.set_computer_enabled(true),
        crate::ComputerCmd::Disable => runtime.set_computer_enabled(false),
        crate::ComputerCmd::Observe { url } => {
            runtime
                .computer_operator_action(
                    json!({"action":"open","agent_id":"operator","url":url}),
                    rx.clone(),
                )
                .await
        }
        crate::ComputerCmd::Session => {
            let result = session(&runtime, rx).await;
            runtime.computer.shutdown().await;
            signal.abort();
            return result;
        }
    };
    signal.abort();
    runtime.computer.shutdown().await;
    println!("{}", serde_json::to_string_pretty(&result?)?);
    Ok(())
}

async fn session(runtime: &Runtime, mut cancel: watch::Receiver<bool>) -> Result<()> {
    eprintln!("ARIS browser session. Start with {{\"action\":\"open\",\"agent_id\":\"researcher\",\"url\":\"https://example.com\"}}. Actions: list, tabs, observe, navigate, click, double_click, move, type, keypress, scroll, drag, wait, new_page, select_page, screenshot, close. Each agent_id owns isolated cookies, pages and a page mouse. Mouse coordinates use the 1440x900 CSS-pixel viewport; keypress uses Playwright keys such as Enter or Control+A. EOF or Ctrl+C closes all contexts.");
    let mut input = BufReader::new(tokio::io::stdin()).lines();
    loop {
        let line = tokio::select! {
            value=input.next_line()=>value?,
            _=cancel.changed()=>break,
        };
        let Some(line) = line else {
            break;
        };
        if line.trim().is_empty() {
            continue;
        }
        let result = match serde_json::from_str::<Value>(&line) {
            Ok(args) => dispatch(runtime, args, cancel.clone()).await,
            Err(error) => Err(error.into()),
        };
        let output = match result {
            Ok(value) => json!({"result":value}),
            Err(error) => json!({"error":{"message":error.to_string()}}),
        };
        println!("{}", serde_json::to_string(&output)?);
    }
    Ok(())
}

async fn dispatch(runtime: &Runtime, args: Value, cancel: watch::Receiver<bool>) -> Result<Value> {
    runtime.computer_operator_action(args, cancel).await
}
