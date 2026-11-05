use super::sanitize_user_agent;
use super::*;
use core_test_support::skip_if_no_network;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn provider_socks_proxy_authenticates_with_url_credentials() {
    use tokio::io::AsyncReadExt;
    use tokio::io::AsyncWriteExt;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = format!("socks5://user:p%40ss@{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut header = [0; 2];
        socket.read_exact(&mut header).await.unwrap();
        let mut methods = vec![0; usize::from(header[1])];
        socket.read_exact(&mut methods).await.unwrap();
        assert!(methods.contains(&2));
        socket.write_all(&[5, 2]).await.unwrap();
        socket.read_exact(&mut header).await.unwrap();
        let mut username = vec![0; usize::from(header[1])];
        socket.read_exact(&mut username).await.unwrap();
        let password_len = socket.read_u8().await.unwrap();
        let mut password = vec![0; usize::from(password_len)];
        socket.read_exact(&mut password).await.unwrap();
        assert_eq!(
            (header[0], username, password),
            (1, b"user".to_vec(), b"p@ss".to_vec())
        );
        socket.write_all(&[1, 0]).await.unwrap();
        let mut connect = [0; 10];
        socket.read_exact(&mut connect).await.unwrap();
        assert_eq!(connect, [5, 1, 0, 1, 127, 0, 0, 1, 48, 57]);
        socket
            .write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 0])
            .await
            .unwrap();
        let mut request = Vec::new();
        while !request.ends_with(b"\r\n\r\n") {
            request.push(socket.read_u8().await.unwrap());
        }
        socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
            .await
            .unwrap();
    });
    let response = build_provider_client(Some(&proxy))
        .unwrap()
        .get("http://127.0.0.1:12345/v1/models")
        .send()
        .await
        .unwrap();
    assert_eq!(response.text().await.unwrap(), "ok");
    server.await.unwrap();
}

#[tokio::test]
async fn provider_client_routes_requests_through_explicit_proxy() {
    let proxy = wiremock::MockServer::start().await;
    let other_proxy = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_string("proxied"))
        .expect(1)
        .mount(&proxy)
        .await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_string("other provider"))
        .expect(1)
        .mount(&other_proxy)
        .await;
    let client = build_provider_client(Some(&proxy.uri())).unwrap();
    let other_client = build_provider_client(Some(&other_proxy.uri())).unwrap();
    let response = client
        .get("http://provider.invalid/v1/models")
        .send()
        .await
        .unwrap();
    assert_eq!(response.text().await.unwrap(), "proxied");
    let response = other_client
        .get("http://provider.invalid/v1/models")
        .send()
        .await
        .unwrap();
    assert_eq!(response.text().await.unwrap(), "other provider");
}

#[test]
fn test_get_codex_user_agent() {
    let user_agent = get_codex_user_agent();
    let originator = originator().value;
    let prefix = format!("{originator}/");
    assert!(user_agent.starts_with(&prefix));
}

#[test]
fn is_first_party_originator_matches_known_values() {
    assert_eq!(is_first_party_originator(DEFAULT_ORIGINATOR), true);
    assert_eq!(is_first_party_originator("codex-tui"), true);
    assert_eq!(is_first_party_originator("codex_vscode"), true);
    assert_eq!(is_first_party_originator("Codex Something Else"), true);
    assert_eq!(is_first_party_originator("codex_cli"), false);
    assert_eq!(is_first_party_originator("Other"), false);
}

#[test]
fn is_first_party_chat_originator_matches_known_values() {
    assert_eq!(is_first_party_chat_originator("codex_atlas"), true);
    assert_eq!(
        is_first_party_chat_originator("codex_chatgpt_desktop"),
        true
    );
    assert_eq!(is_first_party_chat_originator(DEFAULT_ORIGINATOR), false);
    assert_eq!(is_first_party_chat_originator("codex_vscode"), false);
}

#[tokio::test]
async fn test_create_client_sets_default_headers() {
    skip_if_no_network!();

    set_default_client_residency_requirement(Some(ResidencyRequirement::Us));

    use wiremock::Mock;
    use wiremock::MockServer;
    use wiremock::ResponseTemplate;
    use wiremock::matchers::method;
    use wiremock::matchers::path;

    let client = create_client();

    // Spin up a local mock server and capture a request.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let resp = client
        .get(server.uri())
        .send()
        .await
        .expect("failed to send request");
    assert!(resp.status().is_success());

    let requests = server
        .received_requests()
        .await
        .expect("failed to fetch received requests");
    assert!(!requests.is_empty());
    let headers = &requests[0].headers;

    // originator header is set to the provided value
    let originator_header = headers
        .get("originator")
        .expect("originator header missing");
    assert_eq!(originator_header.to_str().unwrap(), originator().value);

    // User-Agent matches the computed Codex UA for that originator
    let expected_ua = get_codex_user_agent();
    let ua_header = headers
        .get("user-agent")
        .expect("user-agent header missing");
    assert_eq!(ua_header.to_str().unwrap(), expected_ua);

    let residency_header = headers
        .get(RESIDENCY_HEADER_NAME)
        .expect("residency header missing");
    assert_eq!(residency_header.to_str().unwrap(), "us");

    set_default_client_residency_requirement(/*enforce_residency*/ None);
}

#[test]
fn test_invalid_suffix_is_sanitized() {
    let prefix = "codex_cli_rs/0.0.0";
    let suffix = "bad\rsuffix";

    assert_eq!(
        sanitize_user_agent(format!("{prefix} ({suffix})"), prefix),
        "codex_cli_rs/0.0.0 (bad_suffix)"
    );
}

#[test]
fn test_invalid_suffix_is_sanitized2() {
    let prefix = "codex_cli_rs/0.0.0";
    let suffix = "bad\0suffix";

    assert_eq!(
        sanitize_user_agent(format!("{prefix} ({suffix})"), prefix),
        "codex_cli_rs/0.0.0 (bad_suffix)"
    );
}

#[test]
#[cfg(target_os = "macos")]
fn test_macos() {
    use regex_lite::Regex;
    let user_agent = get_codex_user_agent();
    let originator = regex_lite::escape(originator().value.as_str());
    let re = Regex::new(&format!(
        r"^{originator}/\d+\.\d+\.\d+ \(Mac OS \d+\.\d+\.\d+; (x86_64|arm64)\) (\S+)$"
    ))
    .unwrap();
    assert!(re.is_match(&user_agent));
}
