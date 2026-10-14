use mio_coding_agent::{
    AgentEvent, Config, Host, HostFuture, Provider, RunOptions, Services, ThinkingLevel,
    run_with_options,
};
use serde_json::{Value, json};
use std::{
    io::Write,
    net::TcpListener,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};
#[path = "../src/test_http.rs"]
mod test_http;
struct QuietHost;
impl Host for QuietHost {
    fn shell(&self, _: &str, _: &Path) -> HostFuture<Result<String, String>> {
        Box::pin(async { Err("unexpected shell".into()) })
    }
    fn on_event(&self, _: AgentEvent) {}
}
async fn capture(provider: Provider, model: &str, thinking: Option<ThinkingLevel>) -> Value {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let server = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut socket = loop {
            match listener.accept() {
                Ok((socket, _)) => break socket,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "no request");
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(e) => panic!("{e}"),
            }
        };
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let (_, request) = test_http::read_json(&mut socket);
        let chunk = json!({"id":"t","object":"chat.completion.chunk","created":0,"model":"test","choices":[{"index":0,"delta":{"role":"assistant","content":"ok"},"finish_reason":"stop"}]});
        let body = format!("data: {chunk}\n\ndata: [DONE]\n\n");
        write!(socket,"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
        request
    });
    let dir = tempfile::tempdir().unwrap();
    let result = run_with_options(
        Config {
            provider,
            model: model.into(),
            base_url: format!("http://{address}/v1"),
            api_key: "local-test".into(),
        },
        vec![],
        "hello",
        dir.path(),
        Arc::new(QuietHost),
        Services::default(),
        RunOptions {
            thinking,
            ..Default::default()
        },
    )
    .await;
    let request = server.join().unwrap();
    result.unwrap();
    request
}
#[tokio::test]
async fn thinking_encodes_actual_provider_requests() {
    let unset = capture(Provider::Custom, "test", None).await;
    assert!(unset.get("reasoning_effort").is_none());
    for level in [
        ThinkingLevel::Minimal,
        ThinkingLevel::Low,
        ThinkingLevel::Medium,
        ThinkingLevel::High,
        ThinkingLevel::Xhigh,
        ThinkingLevel::Max,
    ] {
        assert_eq!(
            capture(Provider::Custom, "test", Some(level)).await["reasoning_effort"],
            level.as_str()
        );
    }
    assert_eq!(
        capture(Provider::Custom, "test:high", None).await["reasoning_effort"],
        "high"
    );
    assert_eq!(
        capture(Provider::Custom, "test:high", Some(ThinkingLevel::Off)).await["reasoning_effort"],
        "none"
    );
    let off = capture(Provider::Deepseek, "test", Some(ThinkingLevel::Off)).await;
    assert_eq!(off["thinking"]["type"], "disabled");
    assert!(off.get("reasoning_effort").is_none());
    for level in [ThinkingLevel::Low, ThinkingLevel::High, ThinkingLevel::Max] {
        let request = capture(Provider::Deepseek, "test", Some(level)).await;
        assert_eq!(request["thinking"]["type"], "enabled");
        assert_eq!(request["reasoning_effort"], level.as_str());
    }
}
#[tokio::test]
async fn unsupported_thinking_fails_before_network_or_account_lookup() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let dir = tempfile::tempdir().unwrap();
    for provider in [Provider::Deepseek, Provider::DeepseekWeb] {
        let error = run_with_options(
            Config {
                provider,
                model: "test".into(),
                base_url: format!("http://{}", listener.local_addr().unwrap()),
                api_key: "local-test".into(),
            },
            vec![],
            "hello",
            dir.path(),
            Arc::new(QuietHost),
            Services::default(),
            RunOptions {
                thinking: Some(ThinkingLevel::Medium),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
        assert!(error.contains("不支持 medium"), "{error}");
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }
}
