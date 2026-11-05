use super::*;
use crate::AuthProvider;
use crate::Provider;
use crate::ResponsesClient;
use crate::ResponsesOptions;
use crate::RetryConfig;
use codex_client::ReqwestTransport;
use http::HeaderMap;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

struct NoAuth;
impl AuthProvider for NoAuth {
    fn add_auth_headers(&self, _headers: &mut HeaderMap) {}
}

#[tokio::test]
async fn streams_tools_and_replays_reasoning_with_chat_content() {
    let server = MockServer::start().await;
    let chunks = [
        json!({"id":"chat-1","choices":[{"delta":{"reasoning_content":"inspect first"}}]}),
        json!({"id":"chat-1","choices":[{"delta":{"tool_calls":[{"index":0,"id":"call-1","function":{"name":"read_file","arguments":"{\"path\":"}}]}}]}),
        json!({"id":"chat-1","choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"a\"}"}}]},"finish_reason":"tool_calls"}]}),
    ];
    let mut sse = chunks
        .iter()
        .map(|chunk| format!("data: {chunk}\n\n"))
        .collect::<String>();
    sse.push_str("data: [DONE]\n\n");
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse, "text/event-stream"))
        .expect(1)
        .mount(&server)
        .await;
    let provider = Provider {
        name: "deepseek".into(),
        base_url: format!("{}/v1", server.uri()),
        query_params: None,
        headers: HeaderMap::new(),
        retry: RetryConfig {
            max_attempts: 1,
            base_delay: Duration::from_millis(1),
            retry_429: false,
            retry_5xx: false,
            retry_transport: false,
        },
        stream_idle_timeout: Duration::from_secs(2),
    };
    let mut request = ResponsesApiRequest {
        model:"deepseek-v4.1-flash-1017-heretic".into(), instructions:"help".into(),
        input:vec![serde_json::from_value(json!({"type":"message","role":"user","content":[{"type":"input_text","text":"read a"},{"type":"input_image","image_url":"data:image/png;base64,abc"}]})).unwrap()],
        tools:vec![json!({"type":"function","name":"read_file","parameters":{"type":"object"}})],
        tool_choice:"auto".into(), parallel_tool_calls:false, reasoning:None, store:false, stream:true,
        include:vec![], service_tier:None, prompt_cache_key:None, text:None, client_metadata:None,
    };
    let client = ResponsesClient::new(
        ReqwestTransport::new(reqwest::Client::builder().no_proxy().build().unwrap()),
        provider,
        Arc::new(NoAuth),
    )
    .with_chat_completions();
    let mut stream = client
        .stream_request(request.clone(), ResponsesOptions::default())
        .await
        .unwrap();
    let mut completed = false;
    while let Some(event) = stream.next().await {
        match event.unwrap() {
            ResponseEvent::OutputItemDone(item) => request.input.push(item),
            ResponseEvent::Completed { end_turn, .. } => {
                assert_eq!(end_turn, Some(false));
                completed = true;
            }
            _ => {}
        }
    }
    assert!(completed);
    request.input.push(
        serde_json::from_value(
            json!({"type":"function_call_output","call_id":"call-1","output":"contents"}),
        )
        .unwrap(),
    );
    let body = request_body(&request).unwrap();
    assert_eq!(
        body["messages"],
        json!([
            {"role":"system","content":"help"},
            {"role":"user","content":[{"type":"text","text":"read a"},{"type":"image_url","image_url":{"url":"data:image/png;base64,abc"}}]},
            {"role":"assistant","content":null,"reasoning_content":"inspect first","tool_calls":[{"id":"call-1","type":"function","function":{"name":"read_file","arguments":"{\"path\":\"a\"}"}}]},
            {"role":"tool","tool_call_id":"call-1","content":"contents"}
        ])
    );
    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        requests[0].body_json::<Value>().unwrap()["messages"],
        json!(body["messages"].as_array().unwrap()[..2])
    );
}

#[tokio::test]
async fn truncated_stream_is_an_error() {
    let response = StreamResponse {
        status: http::StatusCode::OK,
        headers: HeaderMap::new(),
        bytes: Box::pin(futures::stream::iter(vec![Ok(bytes::Bytes::from_static(
            b"data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n",
        ))])),
    };
    let mut stream = stream(response, Duration::from_secs(2), vec![]);
    let mut failed = false;
    while let Some(event) = stream.next().await {
        if event.is_err() {
            failed = true;
        }
        assert!(!matches!(event, Ok(ResponseEvent::Completed { .. })));
    }
    assert!(failed);
}
