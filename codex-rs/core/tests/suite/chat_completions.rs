use codex_model_provider_info::WireApi;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::user_input::UserInput;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_provider_uses_chat_endpoint_through_proxy() {
    let server = MockServer::start().await;
    let chunk = json!({"id":"chat-test", "choices":[{"delta":{"content":"hello"}, "finish_reason":"stop"}]});
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            format!("data: {chunk}\n\ndata: [DONE]\n\n"),
            "text/event-stream",
        ))
        .expect(1)
        .mount(&server)
        .await;
    let proxy_url = server.uri();
    let test = test_codex()
        .with_config(move |config| {
            config.model_provider.base_url = Some("http://provider.invalid/v1".into());
            config.model_provider.wire_api = WireApi::Chat;
            config.model_provider.proxy = Some(proxy_url);
            config.model_provider.supports_websockets = true;
        })
        .build(&server)
        .await
        .unwrap();
    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "hello".into(),
                text_elements: vec![],
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await
        .unwrap();
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let requests = server.received_requests().await.unwrap();
    let body: Value = requests
        .iter()
        .find(|request| request.url.path() == "/v1/chat/completions")
        .unwrap()
        .body_json()
        .unwrap();
    assert_eq!(
        body["messages"].as_array().unwrap().last().unwrap(),
        &json!({"role":"user","content":"hello"})
    );
}
