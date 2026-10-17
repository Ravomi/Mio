//! A host-supplied output schema must reach a real model request, and an
//! oversized schema must fail before the model is contacted.

#[path = "../src/test_http.rs"]
mod test_http;

use mio_coding_agent::{
    AgentEvent, Config, Host, HostFuture, OutputSchema, Provider, RunOptions, Services, ToolFilter,
    run_with_tools,
};
use serde_json::{Value, json};
use std::{
    io::Write,
    net::{TcpListener, TcpStream},
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Default)]
struct RecordingHost {
    events: Mutex<Vec<AgentEvent>>,
    schema: Option<OutputSchema>,
}

impl Host for RecordingHost {
    fn output_schema(&self) -> Option<OutputSchema> {
        self.schema.clone()
    }

    fn shell(&self, _command: &str, _cwd: &Path) -> HostFuture<Result<String, String>> {
        Box::pin(async { Err("shell is unavailable in this test".into()) })
    }

    fn on_event(&self, event: AgentEvent) {
        self.events.lock().unwrap().push(event);
    }
}

fn host_with_schema(schema: OutputSchema) -> Arc<RecordingHost> {
    Arc::new(RecordingHost {
        schema: Some(schema),
        ..RecordingHost::default()
    })
}

fn schema() -> OutputSchema {
    OutputSchema::try_from(json!({
        "type": "object",
        "properties": {"summary": {"type": "string"}},
        "required": ["summary"],
        "additionalProperties": false
    }))
    .unwrap()
}

fn config(address: std::net::SocketAddr) -> Config {
    Config {
        provider: Provider::Custom,
        base_url: format!("http://{address}/v1"),
        model: "schema-loopback".into(),
        api_key: "local-test-key".into(),
    }
}

fn write_response(socket: &mut TcpStream) {
    let delta = json!({
        "id": "schema-test",
        "object": "chat.completion.chunk",
        "created": 0,
        "model": "schema-loopback",
        "choices": [{"index": 0, "delta": {"role": "assistant", "content": "{\"summary\":\"checked\"}"}, "finish_reason": null}]
    });
    let finish = json!({
        "id": "schema-test",
        "object": "chat.completion.chunk",
        "created": 0,
        "model": "schema-loopback",
        "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]
    });
    let body = format!("data: {delta}\n\ndata: {finish}\n\ndata: [DONE]\n\n");
    write!(
        socket,
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    socket.flush().unwrap();
}

fn write_tool_call(socket: &mut TcpStream, name: &str, id: &str, arguments: Value) {
    let delta = json!({
        "id": "schema-tool-test",
        "object": "chat.completion.chunk",
        "created": 0,
        "model": "schema-loopback",
        "choices": [{"index": 0, "delta": {"role": "assistant", "tool_calls": [{
            "index": 0,
            "id": id,
            "type": "function",
            "function": {"name": name, "arguments": arguments.to_string()}
        }]}, "finish_reason": null}]
    });
    let finish = json!({
        "id": "schema-tool-test",
        "object": "chat.completion.chunk",
        "created": 0,
        "model": "schema-loopback",
        "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]
    });
    let body = format!("data: {delta}\n\ndata: {finish}\n\ndata: [DONE]\n\n");
    write!(
        socket,
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    socket.flush().unwrap();
}

#[tokio::test]
async fn sdk_output_schema_reaches_the_model_request() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || -> Value {
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let (_, request) = test_http::read_json(&mut socket);
        write_response(&mut socket);
        request
    });
    let workspace = tempfile::tempdir().unwrap();
    let host = host_with_schema(schema());
    let tools = mio_coding_agent::RunTools {
        tool_filter: ToolFilter::none(),
        ..Default::default()
    };
    let history = tokio::time::timeout(
        Duration::from_secs(20),
        run_with_tools(
            config(address),
            Vec::new(),
            "Return a structured summary.",
            workspace.path(),
            host.clone(),
            Services::default(),
            tools,
        ),
    )
    .await
    .expect("run timed out")
    .expect("structured run failed");
    assert!(!history.is_empty());
    let request = server.join().unwrap();
    assert_eq!(request["response_format"]["type"], "json_schema");
    assert_eq!(
        request["response_format"]["json_schema"]["schema"]["required"],
        json!(["summary"])
    );
    assert!(
        host.events
            .lock()
            .unwrap()
            .iter()
            .any(|event| matches!(event, AgentEvent::Text(text) if text.contains("checked")))
    );
}

#[tokio::test]
async fn sdk_output_schema_survives_a_real_read_tool_round_trip() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || -> (Value, Value, Option<String>) {
        let (mut first_socket, _) = listener.accept().unwrap();
        first_socket
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let (_, first) = test_http::read_json(&mut first_socket);
        let output_tool = first["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|tool| tool["function"]["name"].as_str())
            .find(|name| *name != "read")
            .map(str::to_owned);
        write_tool_call(
            &mut first_socket,
            "read",
            "call-read-seed",
            json!({"path":"seed.txt"}),
        );
        drop(first_socket);

        let (mut second_socket, _) = listener.accept().unwrap();
        second_socket
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let (_, second) = test_http::read_json(&mut second_socket);
        if let Some(output_tool) = &output_tool {
            write_tool_call(
                &mut second_socket,
                output_tool,
                "call-structured-result",
                json!({"summary":"observed-seed"}),
            );
        } else {
            write_response(&mut second_socket);
        }
        (first, second, output_tool)
    });
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("seed.txt"), "observed-seed\n").unwrap();
    let host = host_with_schema(schema());
    let tools = mio_coding_agent::RunTools {
        tool_filter: ToolFilter::from_whitelist(&["read".into()]).unwrap(),
        ..Default::default()
    };
    let history = tokio::time::timeout(
        Duration::from_secs(20),
        run_with_tools(
            config(address),
            Vec::new(),
            "Read seed.txt and return a structured summary.",
            workspace.path(),
            host,
            Services::default(),
            tools,
        ),
    )
    .await
    .expect("run timed out")
    .expect("structured read run failed");
    let (first, second, output_tool) = server.join().unwrap();
    if let Some(output_tool) = output_tool {
        assert_ne!(output_tool, "read");
    } else {
        // This dialect defers native response_format until a tool result
        // exists, so the first request stays callable.
        assert!(first["response_format"].is_null());
        assert_eq!(second["response_format"]["type"], "json_schema");
    }
    assert!(
        first["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["function"]["name"] == "read")
    );
    assert!(second["messages"].as_array().unwrap().iter().any(
        |message| message["role"] == "tool" && message["tool_call_id"] == "call-read-seed"
    ));
    assert!(second["messages"].to_string().contains("observed-seed"));
    assert!(
        serde_json::to_string(&history).unwrap().contains("checked")
            || serde_json::to_string(&history)
                .unwrap()
                .contains("observed-seed")
    );
    assert_eq!(
        std::fs::read(workspace.path().join("seed.txt")).unwrap(),
        b"observed-seed\n"
    );
}

#[tokio::test]
async fn oversized_schema_is_refused_before_a_model_request() {
    let workspace = tempfile::tempdir().unwrap();
    let oversized = OutputSchema::try_from(json!({
        "type": "object",
        "description": "x".repeat(33 * 1024)
    }))
    .unwrap();
    let host = host_with_schema(oversized);
    let options = RunOptions::default();
    let result = mio_coding_agent::run_with_options(
        config("127.0.0.1:9".parse().unwrap()),
        Vec::new(),
        "No request should be sent.",
        workspace.path(),
        host,
        Services::default(),
        options,
    )
    .await;
    assert!(result.unwrap_err().contains("32 KiB"));
}
