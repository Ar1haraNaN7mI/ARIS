//! OpenAI Responses wire adapter for KEI gateways exposing /responses.
//! This is an API-key transport, not a Codex CLI login/session implementation.
use crate::{
    AssembledResponse, ChatMessage, ContentPart, FinishReason, FunctionCallDelta, LlmConfig,
    LlmError, LlmEvent, LlmRequestOptions, Role, ToolCallDelta, ToolSpec,
};
use eventsource_stream::Eventsource;
use futures::StreamExt;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use tokio::sync::{mpsc, watch};

pub(crate) fn response_url(base: &str) -> String {
    let base = base.trim_end_matches('/');
    let base = base.strip_suffix("/chat/completions").unwrap_or(base);
    if base.ends_with("/responses") {
        base.into()
    } else {
        format!("{base}/responses")
    }
}

fn request_body(
    config: &LlmConfig,
    messages: &[ChatMessage],
    tools: &[ToolSpec],
    options: Option<&LlmRequestOptions>,
) -> Value {
    let mut input = Vec::new();
    for message in messages {
        if let Some(items) = &message.provider_items {
            input.extend(
                items
                    .iter()
                    .filter(|item| item["type"] == "reasoning")
                    .cloned(),
            );
        }
        if message.role == Role::Tool {
            input.push(json!({"type":"function_call_output", "call_id":message.tool_call_id, "output":message.text()}));
            continue;
        }
        let role = match message.role {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => unreachable!(),
        };
        let content = match &message.content {
            Some(ContentPart::Text(text)) if !text.is_empty() => vec![json!({"type":if message.role == Role::Assistant {"output_text"} else {"input_text"},"text":text})],
            Some(ContentPart::Parts(parts)) => parts.iter().filter_map(|part| {
                match part.get("type").and_then(Value::as_str) {
                    Some("text") => Some(json!({"type":"input_text","text":part["text"]})),
                    Some("image_url") => Some(json!({"type":"input_image","image_url":part["image_url"]["url"],"detail":part["image_url"].get("detail").cloned().unwrap_or(json!("auto"))})),
                    _ => None,
                }
            }).collect(),
            _ => Vec::new(),
        };
        if !content.is_empty() {
            if message.role == Role::Assistant {
                input.push(json!({"role":role,"content":message.text()}));
            } else {
                input.push(json!({"role":role,"content":content}));
            }
        }
        for call in message.tool_calls.as_deref().unwrap_or_default() {
            if let Some(function) = &call.function {
                input.push(json!({"type":"function_call","call_id":call.id,"name":function.name,"arguments":function.arguments.as_deref().unwrap_or("{}") }));
            }
        }
    }
    let mut body = json!({"model":config.model,"input":input,"stream":true,"store":false,"include":["reasoning.encrypted_content"],
        "max_output_tokens":options.and_then(|o| o.max_tokens).unwrap_or(config.max_tokens)});
    // Reasoning deployments often reject temperature. The field is optional in
    // Responses, so send it only for profiles explicitly running without thinking.
    if !options.and_then(|o| o.thinking).unwrap_or(config.thinking) {
        body["temperature"] = json!(options
            .and_then(|o| o.temperature)
            .unwrap_or(config.temperature));
    }
    if !tools.is_empty() {
        body["tools"] = json!(tools.iter().map(|tool| json!({"type":"function","name":tool.function.name,"description":tool.function.description,"parameters":tool.function.parameters,"strict":false})).collect::<Vec<_>>());
        body["tool_choice"] = json!("auto");
        if let Some(value) = options.and_then(|o| o.parallel_tool_calls) {
            body["parallel_tool_calls"] = json!(value);
        }
    }
    for extra in [
        config.extra_body.as_ref(),
        options.and_then(|o| o.extra_body.as_ref()),
    ]
    .into_iter()
    .flatten()
    {
        if let Some(object) = extra.as_object() {
            for (key, value) in object {
                if !matches!(
                    key.as_str(),
                    "model"
                        | "input"
                        | "stream"
                        | "store"
                        | "tools"
                        | "tool_choice"
                        | "max_output_tokens"
                        | "previous_response_id"
                        | "conversation"
                        | "instructions"
                ) {
                    body[key] = value.clone();
                }
            }
        }
    }
    body
}

async fn cancelled(cancel: &mut Option<watch::Receiver<bool>>) {
    if let Some(receiver) = cancel {
        loop {
            if *receiver.borrow() {
                return;
            }
            if receiver.changed().await.is_err() {
                std::future::pending::<()>().await;
            }
        }
    } else {
        std::future::pending::<()>().await;
    }
}

pub(crate) async fn stream(
    http: &reqwest::Client,
    config: LlmConfig,
    messages: &[ChatMessage],
    tools: &[ToolSpec],
    tx: mpsc::Sender<LlmEvent>,
    mut cancel: Option<watch::Receiver<bool>>,
    options: Option<LlmRequestOptions>,
    send_local_key: bool,
) -> Result<AssembledResponse, LlmError> {
    let mut candidates = vec![config.clone()];
    for endpoint in &config.fallbacks {
        let mut fallback = config.clone();
        fallback.base_url = endpoint.base_url.clone();
        fallback.model = endpoint
            .model
            .clone()
            .unwrap_or_else(|| config.model.clone());
        fallback.backend = endpoint.backend.unwrap_or(config.backend);
        fallback.api_key = endpoint
            .api_key
            .clone()
            .or_else(|| {
                endpoint
                    .api_key_env
                    .as_deref()
                    .and_then(|name| std::env::var(name).ok())
            })
            .unwrap_or_else(|| config.api_key.clone());
        if endpoint.extra_body.is_some() {
            fallback.extra_body = endpoint.extra_body.clone();
        }
        candidates.push(fallback);
    }
    let mut response = None;
    let mut error = LlmError::Http("No Responses endpoint configured".into());
    for candidate in candidates {
        if candidate.backend.requires_api_key() && candidate.api_key.trim().is_empty() {
            error = LlmError::MissingApiKey;
            continue;
        }
        let url = response_url(&candidate.base_url);
        let mut request =
            http.post(&url)
                .json(&request_body(&candidate, messages, tools, options.as_ref()));
        let local = reqwest::Url::parse(&url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_owned))
            .is_some_and(|h| {
                h == "localhost" || h == "::1" || h == "[::1]" || h.starts_with("127.")
            });
        if !candidate.api_key.is_empty() && (!local || send_local_key) {
            request = request.bearer_auth(&candidate.api_key);
        }
        let sent = tokio::select! { result = request.send() => result, _ = cancelled(&mut cancel) => return Err(LlmError::Api("cancelled".into())) };
        match sent {
            Ok(value) if value.status().is_success() => {
                response = Some(value);
                break;
            }
            Ok(value) => {
                error = LlmError::Api(format!("Responses endpoint returned {}", value.status()));
            }
            Err(value) => {
                error = LlmError::Http(value.to_string());
            }
        }
    }
    let response = response.ok_or(error)?;
    if response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("application/json"))
    {
        let value: Value = tokio::select! {
            value = response.json() => value.map_err(|e|LlmError::Api(e.to_string()))?,
            _ = cancelled(&mut cancel) => return Err(LlmError::Api("cancelled".into()))
        };
        let result = assemble(&value)?;
        emit_complete(&result, &tx).await;
        return Ok(result);
    }
    let mut stream = response.bytes_stream().eventsource();
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut calls: BTreeMap<usize, ToolCallDelta> = BTreeMap::new();
    loop {
        let next = tokio::select! { item=stream.next() => item, _=cancelled(&mut cancel) => return Err(LlmError::Api("cancelled".into())) };
        let Some(event) = next else {
            return Err(LlmError::Api(
                "Responses stream ended before a terminal response event".into(),
            ));
        };
        let event = event.map_err(|e| LlmError::Http(e.to_string()))?;
        let value: Value = serde_json::from_str(&event.data)
            .map_err(|e| LlmError::Api(format!("Invalid Responses event: {e}")))?;
        match value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or(&event.event)
        {
            "response.output_text.delta" => {
                let delta = value["delta"].as_str().unwrap_or_default();
                text.push_str(delta);
                let _ = tx.send(LlmEvent::TextDelta(delta.into())).await;
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                let delta = value["delta"].as_str().unwrap_or_default();
                reasoning.push_str(delta);
                let _ = tx.send(LlmEvent::ReasoningDelta(delta.into())).await;
            }
            "response.output_item.added" | "response.output_item.done"
                if value["item"]["type"] == "function_call" =>
            {
                let index = value["output_index"].as_u64().unwrap_or(0) as usize;
                let item = &value["item"];
                let call = ToolCallDelta {
                    index: Some(index),
                    id: item["call_id"].as_str().map(str::to_owned),
                    r#type: Some("function".into()),
                    function: Some(FunctionCallDelta {
                        name: item["name"].as_str().map(str::to_owned),
                        arguments: item["arguments"].as_str().map(str::to_owned),
                    }),
                };
                calls.insert(index, call);
            }
            "response.function_call_arguments.delta" => {
                let index = value["output_index"].as_u64().unwrap_or(0) as usize;
                let call = calls.entry(index).or_default();
                let function = call.function.get_or_insert_with(Default::default);
                function
                    .arguments
                    .get_or_insert_with(String::new)
                    .push_str(value["delta"].as_str().unwrap_or_default());
            }
            "response.completed" | "response.incomplete" => {
                let mut result = assemble(&value["response"])?;
                if result.text.is_empty() {
                    result.text = text.clone();
                }
                if result.reasoning.is_none() && !reasoning.is_empty() {
                    result.reasoning = Some(reasoning);
                }
                if result.tool_calls.is_empty() {
                    result.tool_calls = calls.into_values().collect();
                }
                if result.finish_reason == FinishReason::Length && !result.tool_calls.is_empty() {
                    return Err(LlmError::IncompleteToolCalls(result.tool_calls));
                }
                if !result.tool_calls.is_empty() && result.finish_reason != FinishReason::Length {
                    result.finish_reason = FinishReason::ToolCalls;
                }
                if text.is_empty() && !result.text.is_empty() {
                    let _ = tx.send(LlmEvent::TextDelta(result.text.clone())).await;
                }
                for call in &result.tool_calls {
                    let _ = tx.send(LlmEvent::ToolCallDelta(call.clone())).await;
                }
                let _ = tx
                    .send(LlmEvent::Finished(result.finish_reason.clone()))
                    .await;
                return Ok(result);
            }
            "response.failed" | "error" => {
                return Err(LlmError::Api(
                    "Responses API reported a failed response; inspect the gateway request log"
                        .into(),
                ));
            }
            _ => {}
        }
    }
}

fn assemble(value: &Value) -> Result<AssembledResponse, LlmError> {
    if value.get("error").is_some_and(|v| !v.is_null()) || value["status"] == "failed" {
        return Err(LlmError::Api("Responses API returned an error".into()));
    }
    let output = value
        .get("output")
        .and_then(Value::as_array)
        .ok_or_else(|| LlmError::Api("Responses payload has no output array".into()))?;
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut tool_calls = Vec::new();
    for (index, item) in output.iter().enumerate() {
        match item["type"].as_str().unwrap_or_default() {
            "message" => {
                for content in item["content"].as_array().into_iter().flatten() {
                    if content["type"] == "output_text" {
                        text.push_str(content["text"].as_str().unwrap_or_default());
                    }
                    if content["type"] == "refusal" {
                        text.push_str(content["refusal"].as_str().unwrap_or_default());
                    }
                }
            }
            "reasoning" => {
                for part in item["summary"].as_array().into_iter().flatten() {
                    reasoning.push_str(part["text"].as_str().unwrap_or_default());
                }
            }
            "function_call" => tool_calls.push(ToolCallDelta {
                index: Some(index),
                id: item["call_id"].as_str().map(str::to_owned),
                r#type: Some("function".into()),
                function: Some(FunctionCallDelta {
                    name: item["name"].as_str().map(str::to_owned),
                    arguments: item["arguments"].as_str().map(str::to_owned),
                }),
            }),
            _ => {}
        }
    }
    let finish_reason = if value["status"] == "incomplete" {
        FinishReason::Length
    } else if !tool_calls.is_empty() {
        FinishReason::ToolCalls
    } else {
        FinishReason::Stop
    };
    if finish_reason == FinishReason::Length && !tool_calls.is_empty() {
        return Err(LlmError::IncompleteToolCalls(tool_calls));
    }
    let provider_items: Vec<Value> = output
        .iter()
        .filter(|item| item["type"] == "reasoning")
        .cloned()
        .collect();
    Ok(AssembledResponse {
        text,
        reasoning: if reasoning.is_empty() {
            None
        } else {
            Some(reasoning)
        },
        tool_calls,
        finish_reason,
        provider_items: if provider_items.is_empty() {
            None
        } else {
            Some(provider_items)
        },
    })
}

async fn emit_complete(result: &AssembledResponse, tx: &mpsc::Sender<LlmEvent>) {
    if !result.text.is_empty() {
        let _ = tx.send(LlmEvent::TextDelta(result.text.clone())).await;
    }
    if let Some(reasoning) = &result.reasoning {
        let _ = tx.send(LlmEvent::ReasoningDelta(reasoning.clone())).await;
    }
    for call in &result.tool_calls {
        let _ = tx.send(LlmEvent::ToolCallDelta(call.clone())).await;
    }
    let _ = tx
        .send(LlmEvent::Finished(result.finish_reason.clone()))
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn projects_tool_roundtrip_and_images_to_responses() {
        let mut assistant = ChatMessage::assistant("");
        assistant.tool_calls = Some(vec![ToolCallDelta {
            index: Some(0),
            id: Some("call-one".into()),
            r#type: Some("function".into()),
            function: Some(FunctionCallDelta {
                name: Some("read_file".into()),
                arguments: Some("{}".into()),
            }),
        }]);
        let mut image = ChatMessage::user("");
        image.content = Some(ContentPart::Parts(vec![
            json!({"type":"image_url","image_url":{"url":"data:image/png;base64,AAAA"}}),
        ]));
        let body = request_body(
            &LlmConfig::default(),
            &[
                ChatMessage::system("role"),
                assistant,
                ChatMessage::tool_result("call-one", "file content"),
                image,
            ],
            &[],
            None,
        );
        assert_eq!(body["input"][1]["type"], "function_call");
        assert_eq!(body["input"][2]["call_id"], "call-one");
        assert_eq!(body["input"][3]["content"][0]["type"], "input_image");
        assert_eq!(
            response_url("https://gateway/v1/chat/completions"),
            "https://gateway/v1/responses"
        );
        assert!(body.get("temperature").is_none());
    }
    #[tokio::test]
    async fn streams_real_http_function_calls() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = vec![0; 16384];
            let n = socket.read(&mut request).await.unwrap();
            let request = String::from_utf8_lossy(&request[..n]);
            assert!(request.starts_with("POST /v1/responses"));
            assert!(request.contains("upstream-test"));
            let done = json!({"type":"response.completed","response":{"status":"completed","output":[{"type":"function_call","call_id":"call_a","name":"read_file","arguments":"{\"path\":\"a.txt\"}"}]}});
            let body = format!("data: {}\n\n", done);
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",body.len(),body).as_bytes()).await.unwrap();
        });
        let config = LlmConfig {
            base_url: format!("http://{address}/v1"),
            model: "upstream-test".into(),
            backend: crate::LlmBackend::OpenAiCompatible,
            protocol: crate::LlmProtocol::Responses,
            ..Default::default()
        };
        let client = crate::OpenAiCompatibleClient::new(config).unwrap();
        let result = client
            .complete_once(&[ChatMessage::user("Read a file")], &[])
            .await
            .unwrap();
        assert_eq!(result.parsed_tool_calls().unwrap()[0].1, "read_file");
        assert_eq!(result.finish_reason, FinishReason::ToolCalls);
        server.await.unwrap();
    }

    #[test]
    fn encrypted_reasoning_is_replayed_only_in_responses_and_partial_calls_fail() {
        let output = json!({"status":"completed","output":[
            {"type":"reasoning","id":"rs_one","summary":[],"encrypted_content":"opaque-fixture"},
            {"type":"function_call","call_id":"call_one","name":"read_file","arguments":"{}"}
        ]});
        let response = assemble(&output).unwrap();
        let assistant = response.as_assistant_message();
        assert!(!serde_json::to_string(&assistant)
            .unwrap()
            .contains("opaque-fixture"));
        let body = request_body(
            &LlmConfig::default(),
            &[assistant, ChatMessage::tool_result("call_one", "done")],
            &[],
            None,
        );
        assert_eq!(body["input"][0]["encrypted_content"], "opaque-fixture");
        assert_eq!(body["input"][1]["type"], "function_call");
        assert_eq!(body["input"][2]["type"], "function_call_output");
        let mut incomplete = output;
        incomplete["status"] = json!("incomplete");
        assert!(assemble(&incomplete).is_err());
    }

    #[tokio::test]
    async fn truncated_response_stream_is_an_error() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 8192];
            socket.read(&mut request).await.unwrap();
            let body = "data: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\n";
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",body.len(),body).as_bytes()).await.unwrap();
        });
        let client = crate::OpenAiCompatibleClient::new(LlmConfig {
            base_url: format!("http://{address}/v1"),
            model: "offline".into(),
            backend: crate::LlmBackend::OpenAiCompatible,
            protocol: crate::LlmProtocol::Responses,
            ..Default::default()
        })
        .unwrap();
        let error = client
            .complete_once(&[ChatMessage::user("fixture")], &[])
            .await
            .unwrap_err();
        assert!(error.to_string().contains("terminal response"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn cancellation_interrupts_responses_waiting_for_headers() {
        use tokio::io::AsyncReadExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 8192];
            socket.read(&mut request).await.unwrap();
            ready_tx.send(()).unwrap();
            std::future::pending::<()>().await;
        });
        let client = crate::OpenAiCompatibleClient::new(LlmConfig {
            base_url: format!("http://{address}/v1"),
            model: "offline".into(),
            backend: crate::LlmBackend::OpenAiCompatible,
            protocol: crate::LlmProtocol::Responses,
            ..Default::default()
        })
        .unwrap();
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let (tx, _rx) = mpsc::channel(32);
        let task = tokio::spawn(async move {
            client
                .stream_chat_cancellable(&[ChatMessage::user("fixture")], &[], tx, Some(cancel_rx))
                .await
        });
        ready_rx.await.unwrap();
        cancel_tx.send(true).unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap();
        assert!(result.unwrap_err().to_string().contains("cancelled"));
        server.abort();
    }
}
