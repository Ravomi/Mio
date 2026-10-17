//! Sampling must be forwarded explicitly or refused before dispatch.
use mio_coding_agent::{
    AgentEvent, Config, Host, HostFuture, Provider, RunOptions, RunTools, Services, ToolFilter,
    run_with_tools, validate_model_temperature,
};
use serde_json::json;
use std::{io::Write, net::TcpListener, path::Path, sync::Arc};
#[path = "../src/test_http.rs"]
mod test_http;

struct NoTools;
impl Host for NoTools {
    fn shell(&self, _: &str, _: &Path) -> HostFuture<Result<String, String>> {
        Box::pin(async { panic!("no tool execution expected") })
    }
    fn on_event(&self, _: AgentEvent) {}
}

#[test]
fn invalid_or_unsupported_temperature_is_refused_locally() {
    let mut config = Config {
        model: "test".into(),
        ..Config::default()
    };
    for temperature in [f64::NAN, f64::INFINITY, -0.1, 2.1] {
        assert!(validate_model_temperature(&config, Some(temperature), None, None).is_err());
    }
    for provider in [Provider::Antigravity, Provider::DeepseekWeb] {
        config.provider = provider;
        assert!(validate_model_temperature(&config, Some(0.0), None, None).is_err());
        assert!(validate_model_temperature(&config, None, None, None).is_ok());
    }
    config.provider = Provider::Custom;
    let model = mio_core::catalog::Catalog::builtin()
        .iter()
        .find(|model| model.sampling == Some(mio_core::catalog::Sampling::Never))
        .unwrap();
    config.model = model.id.clone();
    assert!(
        validate_model_temperature(&config, Some(0.0), None, Some(model.provider.vendor()))
            .is_err()
    );
}

#[tokio::test]
async fn explicit_temperature_and_output_limit_reach_the_wire() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let server = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let (mut stream, _) = loop {
            match listener.accept() {
                Ok(connection) => break connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "model request never reached the fixture"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                Err(error) => panic!("{error}"),
            }
        };
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let (_, request) = test_http::read_json(&mut stream);
        assert_eq!(request["temperature"], 0.0);
        assert_eq!(request["max_tokens"], 128);
        let chunk = json!({"id":"sampling-fixture","object":"chat.completion.chunk","created":0,"model":"test","choices":[{"index":0,"delta":{"content":"ok"},"finish_reason":"stop"}]}).to_string();
        let body = format!("data: {chunk}\n\ndata: [DONE]\n\n");
        write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
    });
    let root = tempfile::tempdir().unwrap();
    let result = run_with_tools(
        Config {
            provider: Provider::Custom,
            model: "test".into(),
            api_key: "local-fixture".into(),
            base_url: format!("http://{address}/v1"),
        },
        vec![],
        "sampling fixture",
        root.path(),
        Arc::new(NoTools),
        Services::default(),
        RunTools {
            options: RunOptions {
                temperature: Some(0.0),
                max_output_tokens: Some(128),
                ..RunOptions::default()
            },
            tool_filter: ToolFilter::none(),
            ..RunTools::default()
        },
    )
    .await;
    server.join().unwrap();
    assert!(result.is_ok(), "{result:?}");
}
