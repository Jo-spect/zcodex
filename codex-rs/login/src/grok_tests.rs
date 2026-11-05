use super::*;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

fn client(home: &std::path::Path, server: &MockServer) -> GrokLogin {
    GrokLogin {
        client: reqwest::Client::builder().no_proxy().build().unwrap(),
        storage: storage::Storage {
            home: home.into(),
            mode: AuthCredentialsStoreMode::File,
        },
        device_url: format!("{}/device", server.uri()),
        token_url: format!("{}/token", server.uri()),
    }
}

#[tokio::test]
async fn device_login_polls_and_saves_separate_credentials() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join("auth.json"),
        "existing ChatGPT credentials",
    )
    .unwrap();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/device"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "device_code":"private-device-code", "user_code":"ABCD-EFGH",
            "verification_uri":"https://accounts.x.ai/oauth2/device", "expires_in":60, "interval":1
        })))
        .expect(1)
        .mount(&server)
        .await;
    let polls = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST")).and(path("/token"))
        .respond_with(move |_request: &wiremock::Request| {
            if polls.fetch_add(1, Ordering::SeqCst) == 0 {
                ResponseTemplate::new(400).set_body_json(json!({"error":"authorization_pending"}))
            } else {
                ResponseTemplate::new(200).set_body_json(json!({"access_token":"access", "refresh_token":"refresh", "expires_in":3600, "token_type":"Bearer"}))
            }
        }).expect(2).mount(&server).await;
    let login = client(home.path(), &server);
    let device = login.begin().await.unwrap();
    insta::assert_snapshot!(device.to_string(), @"Open https://accounts.x.ai/oauth2/device and enter code: ABCD-EFGH");
    assert_eq!(
        (&*device.user_code, &*device.verification_uri),
        ("ABCD-EFGH", "https://accounts.x.ai/oauth2/device")
    );
    login.complete(device).await.unwrap();
    assert_eq!(login.access_token().await.unwrap(), "access");
    assert_eq!(
        std::fs::read_to_string(home.path().join("auth.json")).unwrap(),
        "existing ChatGPT credentials"
    );
    let requests = server.received_requests().await.unwrap();
    let fields = url::form_urlencoded::parse(&requests[0].body)
        .into_owned()
        .collect::<BTreeMap<_, _>>();
    assert_eq!(
        fields,
        BTreeMap::from([
            ("client_id".into(), CLIENT_ID.into()),
            ("scope".into(), SCOPE.into())
        ])
    );
    let fields = url::form_urlencoded::parse(&requests[1].body)
        .into_owned()
        .collect::<BTreeMap<_, _>>();
    assert_eq!(
        fields,
        BTreeMap::from([
            ("client_id".into(), CLIENT_ID.into()),
            ("device_code".into(), "private-device-code".into()),
            (
                "grant_type".into(),
                "urn:ietf:params:oauth:grant-type:device_code".into()
            )
        ])
    );
}

#[tokio::test]
async fn concurrent_expired_token_requests_refresh_once_and_persist_rotation() {
    let home = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let login = client(home.path(), &server);
    login
        .storage
        .save(&Credentials {
            access_token: "expired".into(),
            refresh_token: Some("old-refresh".into()),
            expires_at: 0,
        })
        .unwrap();
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"access_token":"renewed", "refresh_token":"rotated", "expires_in":3600}),
        ))
        .expect(1)
        .mount(&server)
        .await;
    let (first, second) = tokio::join!(login.access_token(), login.access_token());
    assert_eq!(
        (first.unwrap(), second.unwrap()),
        ("renewed".into(), "renewed".into())
    );
    let saved: Value =
        serde_json::from_str(&std::fs::read_to_string(home.path().join("grok-auth.json")).unwrap())
            .unwrap();
    let expires_at = saved["expires_at"].as_i64().unwrap();
    assert!(expires_at > chrono::Utc::now().timestamp() + 3500);
    assert_eq!(
        saved,
        json!({"access_token":"renewed", "refresh_token":"rotated", "expires_at":expires_at})
    );
    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        url::form_urlencoded::parse(&requests[0].body)
            .into_owned()
            .collect::<BTreeMap<_, _>>(),
        BTreeMap::from([
            ("client_id".into(), CLIENT_ID.into()),
            ("grant_type".into(), "refresh_token".into()),
            ("refresh_token".into(), "old-refresh".into())
        ])
    );
}

#[tokio::test]
async fn rejected_refresh_preserves_credentials_and_redacts_response() {
    let home = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let login = client(home.path(), &server);
    login
        .storage
        .save(&Credentials {
            access_token: "expired".into(),
            refresh_token: Some("secret".into()),
            expires_at: 0,
        })
        .unwrap();
    let before = std::fs::read(home.path().join("grok-auth.json")).unwrap();
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(
            ResponseTemplate::new(400)
                .set_body_json(json!({"error":"invalid_grant", "error_description":"secret"})),
        )
        .expect(1)
        .mount(&server)
        .await;
    let error = login.access_token().await.unwrap_err();
    assert_eq!(
        error.to_string(),
        "Grok token refresh rejected; run `codex login grok` again"
    );
    assert_eq!(
        std::fs::read(home.path().join("grok-auth.json")).unwrap(),
        before
    );
}
