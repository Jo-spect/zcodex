//! Converts Chat Completions deployments to the agent's Responses event model.

use crate::common::ResponseEvent;
use crate::common::ResponseStream;
use crate::common::ResponsesApiRequest;
use crate::error::ApiError;
use codex_client::StreamResponse;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ReasoningItemContent;
use codex_protocol::models::ResponseItem;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;
use std::time::Duration;
use tokio::sync::mpsc;

fn invalid(message: impl Into<String>) -> ApiError {
    ApiError::InvalidRequest {
        message: message.into(),
    }
}

fn content(value: &Value) -> Result<Value, ApiError> {
    let Some(parts) = value.as_array() else {
        return Ok(value.clone());
    };
    if parts
        .iter()
        .all(|part| matches!(part["type"].as_str(), Some("input_text" | "output_text")))
    {
        return Ok(json!(
            parts
                .iter()
                .filter_map(|part| part["text"].as_str())
                .collect::<String>()
        ));
    }
    parts
        .iter()
        .map(|part| match part["type"].as_str() {
            Some("input_text" | "output_text") => Ok(json!({"type":"text", "text":part["text"]})),
            Some("input_image") => {
                let mut image = json!({"url":part["image_url"]});
                if let Some(detail) = part.get("detail") {
                    image["detail"] = detail.clone();
                }
                Ok(json!({"type":"image_url", "image_url":image}))
            }
            _ => Err(invalid("unsupported Chat Completions content part")),
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Value::Array)
}

pub(crate) fn request_body(request: &ResponsesApiRequest) -> Result<Value, ApiError> {
    let mut messages: Vec<Value> = Vec::new();
    if !request.instructions.is_empty() {
        messages.push(json!({"role":"system", "content":request.instructions}));
    }
    let mut reasoning = String::new();
    for item in &request.input {
        let item = serde_json::to_value(item).map_err(|e| invalid(e.to_string()))?;
        match item["type"].as_str() {
            Some("reasoning") => {
                if let Some(parts) = item["content"].as_array() {
                    for part in parts {
                        if let Some(text) = part["text"].as_str() {
                            reasoning.push_str(text);
                        }
                    }
                }
            }
            Some("message") => {
                let role = item["role"].as_str().unwrap_or("user");
                let mut message = json!({"role":if role == "developer" { "system" } else { role }, "content":content(&item["content"])?});
                if matches!(role, "assistant" | "system" | "developer")
                    && !message["content"].is_string()
                {
                    return Err(invalid(
                        "Chat Completions only supports text in system and assistant messages",
                    ));
                }
                if role == "assistant" && !reasoning.is_empty() {
                    message["reasoning_content"] = json!(std::mem::take(&mut reasoning));
                }
                messages.push(message);
            }
            Some("function_call" | "custom_tool_call") => {
                let custom = item["type"] == "custom_tool_call";
                let arguments = if custom {
                    json!({"input":item["input"]}).to_string()
                } else {
                    item["arguments"].as_str().unwrap_or("").to_owned()
                };
                let call = json!({"id":item["call_id"], "type":"function", "function":{"name":item["name"], "arguments":arguments}});
                if messages.last().is_none_or(|m| m["role"] != "assistant") {
                    messages.push(json!({"role":"assistant", "content":null}));
                }
                let message = messages
                    .last_mut()
                    .ok_or_else(|| invalid("missing assistant message"))?;
                if !reasoning.is_empty() {
                    message["reasoning_content"] = json!(std::mem::take(&mut reasoning));
                }
                message
                    .as_object_mut()
                    .ok_or_else(|| invalid("invalid assistant message"))?
                    .entry("tool_calls")
                    .or_insert_with(|| json!([]))
                    .as_array_mut()
                    .ok_or_else(|| invalid("invalid tool calls"))?
                    .push(call);
            }
            Some("function_call_output" | "custom_tool_call_output") => {
                messages.push(json!({"role":"tool", "tool_call_id":item["call_id"], "content":content(&item["output"])?}));
            }
            _ => return Err(invalid("unsupported input item for Chat Completions")),
        }
    }
    let mut tools = Vec::new();
    for tool in &request.tools {
        match tool["type"].as_str() {
            Some("function") => {
                let mut function = tool.clone();
                function.as_object_mut().ok_or_else(|| invalid("invalid tool object"))?.remove("type");
                tools.push(json!({"type":"function", "function":function}));
            }
            Some("custom") => tools.push(json!({"type":"function", "function":{
                "name":tool["name"], "description":tool["description"],
                "parameters":{"type":"object", "properties":{"input":{"type":"string"}}, "required":["input"], "additionalProperties":false}
            }})),
            _ => return Err(invalid("Chat Completions requires function tools; disable unsupported built-in tools")),
        }
    }
    let mut body = json!({"model":request.model,"messages":messages,"stream":true,"stream_options":{"include_usage":true}});
    if !tools.is_empty() {
        body["tools"] = json!(tools);
        body["tool_choice"] = json!(request.tool_choice);
    }
    if let Some(reasoning) = &request.reasoning {
        let reasoning = serde_json::to_value(reasoning).map_err(|e| invalid(e.to_string()))?;
        if let Some(effort) = reasoning.get("effort") {
            body["reasoning_effort"] = effort.clone();
        }
    }
    if let Some(format) = request.text.as_ref().and_then(|text| text.format.as_ref()) {
        body["response_format"] = json!({"type":"json_schema", "json_schema":{
            "name":format.name, "schema":format.schema, "strict":format.strict
        }});
    }
    Ok(body)
}

pub(crate) fn stream(
    response: StreamResponse,
    idle_timeout: Duration,
    custom_tools: Vec<String>,
) -> ResponseStream {
    let upstream_request_id = response
        .headers
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let (tx, rx_event) = mpsc::channel(128);
    tokio::spawn(async move {
        let result = consume(response, idle_timeout, custom_tools, &tx).await;
        if let Err(error) = result {
            let _ = tx.send(Err(error)).await;
        }
    });
    ResponseStream {
        rx_event,
        upstream_request_id,
    }
}

fn reasoning_item(id: &str, text: String) -> ResponseItem {
    ResponseItem::Reasoning {
        id: format!("{id}-reasoning"),
        summary: vec![],
        content: Some(vec![ReasoningItemContent::ReasoningText { text }]),
        encrypted_content: None,
    }
}

fn message_item(id: &str, text: String) -> ResponseItem {
    ResponseItem::Message {
        id: Some(format!("{id}-message")),
        role: "assistant".into(),
        content: vec![ContentItem::OutputText { text }],
        phase: None,
    }
}

async fn consume(
    response: StreamResponse,
    idle_timeout: Duration,
    custom_tools: Vec<String>,
    tx: &mpsc::Sender<Result<ResponseEvent, ApiError>>,
) -> Result<(), ApiError> {
    let mut events = response.bytes.eventsource();
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut calls: BTreeMap<u64, Value> = BTreeMap::new();
    let mut response_id = String::new();
    let mut usage = None;
    let mut finished = false;
    let mut end_turn = true;
    let mut reasoning_done = false;
    let _ = tx.send(Ok(ResponseEvent::Created)).await;
    loop {
        let event = tokio::time::timeout(idle_timeout, events.next())
            .await
            .map_err(|_| ApiError::Stream("Chat Completions stream timed out".into()))?;
        let Some(event) = event else {
            break;
        };
        let event = event.map_err(|e| ApiError::Stream(e.to_string()))?;
        if event.data == "[DONE]" {
            break;
        }
        let value: Value =
            serde_json::from_str(&event.data).map_err(|e| ApiError::Stream(e.to_string()))?;
        if let Some(error) = value.get("error") {
            return Err(ApiError::Stream(error.to_string()));
        }
        if let Some(id) = value["id"].as_str() {
            response_id = id.to_owned();
        }
        if value["usage"].is_object() {
            let u = &value["usage"];
            usage = Some(codex_protocol::protocol::TokenUsage {
                input_tokens: u["prompt_tokens"].as_i64().unwrap_or(0),
                cached_input_tokens: u["prompt_tokens_details"]["cached_tokens"]
                    .as_i64()
                    .or_else(|| u["prompt_cache_hit_tokens"].as_i64())
                    .unwrap_or(0),
                output_tokens: u["completion_tokens"].as_i64().unwrap_or(0),
                reasoning_output_tokens: u["completion_tokens_details"]["reasoning_tokens"]
                    .as_i64()
                    .unwrap_or(0),
                total_tokens: u["total_tokens"].as_i64().unwrap_or(0),
            });
        }
        let choice = &value["choices"][0];
        if let Some(reason) = choice["finish_reason"].as_str() {
            if !matches!(reason, "stop" | "tool_calls") {
                return Err(ApiError::Stream(format!(
                    "Chat Completions ended with {reason}"
                )));
            }
            finished = true;
            end_turn = reason == "stop";
        }
        let delta = &choice["delta"];
        if let Some(part) = delta["content"].as_str()
            && !part.is_empty()
        {
            if text.is_empty() {
                if !reasoning.is_empty() {
                    let _ = tx
                        .send(Ok(ResponseEvent::OutputItemDone(reasoning_item(
                            &response_id,
                            reasoning.clone(),
                        ))))
                        .await;
                    reasoning_done = true;
                }
                let _ = tx
                    .send(Ok(ResponseEvent::OutputItemAdded(message_item(
                        &response_id,
                        String::new(),
                    ))))
                    .await;
            }
            text.push_str(part);
            if tx
                .send(Ok(ResponseEvent::OutputTextDelta(part.to_owned())))
                .await
                .is_err()
            {
                return Ok(());
            }
        }
        if let Some(part) = delta["reasoning_content"].as_str()
            && !part.is_empty()
        {
            if reasoning.is_empty() {
                let _ = tx
                    .send(Ok(ResponseEvent::OutputItemAdded(reasoning_item(
                        &response_id,
                        String::new(),
                    ))))
                    .await;
            }
            reasoning.push_str(part);
            if tx
                .send(Ok(ResponseEvent::ReasoningContentDelta {
                    delta: part.to_owned(),
                    content_index: 0,
                }))
                .await
                .is_err()
            {
                return Ok(());
            }
        }
        if let Some(parts) = delta["tool_calls"].as_array() {
            for part in parts {
                let index = part["index"]
                    .as_u64()
                    .ok_or_else(|| invalid("tool call missing index"))?;
                let call = calls
                    .entry(index)
                    .or_insert_with(|| json!({"id":"", "name":"", "arguments":""}));
                if let Some(id) = part["id"].as_str() {
                    call["id"] = json!(id);
                }
                for field in ["name", "arguments"] {
                    if let Some(fragment) = part["function"][field].as_str() {
                        let mut value = call[field].as_str().unwrap_or("").to_owned();
                        value.push_str(fragment);
                        call[field] = json!(value);
                    }
                }
            }
        }
    }
    if !finished {
        return Err(ApiError::Stream(
            "Chat Completions stream ended before finish_reason".into(),
        ));
    }
    let mut items = Vec::new();
    if !reasoning.is_empty() && !reasoning_done {
        let _ = tx
            .send(Ok(ResponseEvent::OutputItemDone(reasoning_item(
                &response_id,
                reasoning,
            ))))
            .await;
    }
    if !text.is_empty() {
        let _ = tx
            .send(Ok(ResponseEvent::OutputItemDone(message_item(
                &response_id,
                text,
            ))))
            .await;
    }
    for call in calls.into_values() {
        if call["id"].as_str().unwrap_or("").is_empty()
            || call["name"].as_str().unwrap_or("").is_empty()
        {
            return Err(invalid("incomplete tool call"));
        }
        if custom_tools.iter().any(|name| call["name"] == *name) {
            let arguments: Value = serde_json::from_str(call["arguments"].as_str().unwrap_or(""))
                .map_err(|e| invalid(e.to_string()))?;
            let input = arguments["input"]
                .as_str()
                .ok_or_else(|| invalid("custom tool call missing input"))?;
            items.push(json!({"type":"custom_tool_call", "call_id":call["id"], "name":call["name"], "input":input}));
        } else {
            items.push(json!({"type":"function_call", "call_id":call["id"], "name":call["name"], "arguments":call["arguments"]}));
        }
    }
    for item in items {
        let item: ResponseItem =
            serde_json::from_value(item).map_err(|e| invalid(e.to_string()))?;
        if tx
            .send(Ok(ResponseEvent::OutputItemDone(item)))
            .await
            .is_err()
        {
            return Ok(());
        }
    }
    let _ = tx
        .send(Ok(ResponseEvent::Completed {
            response_id,
            token_usage: usage,
            end_turn: Some(end_turn),
        }))
        .await;
    Ok(())
}

#[cfg(test)]
#[path = "chat_completions_tests.rs"]
mod tests;
