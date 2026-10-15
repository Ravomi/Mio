# mio-tungstenite

The bundled [`tokio-tungstenite`](https://docs.rs/tokio-tungstenite) websocket backend for Mio: `TungsteniteClient`, a `WebSocketClientExt` implementation. It depends only on `mio-http`, which holds the websocket contract.

`mio-core` owns the websocket *protocol*: the OpenAI Responses session, its event envelopes and its turn state machine all live in `mio_core::providers::openai::responses_api::websocket`, written against the transport-agnostic `mio_http::ws_client` contract (re-exported at `mio_core::ws_client`). This crate owns only the socket, exactly as `mio-reqwest` owns only the HTTP transport. With mio-core's `tungstenite` feature, a session opens over this backend with no backend named:

```rust,ignore
let session = model.responses_websocket().connect().await?;
```

Without it, pass any backend: `model.responses_websocket().connect_with(&TungsteniteClient::new())`.

It also works when the caller has no tokio runtime (Bevy task pools, smol, `futures::executor`): the socket moves onto a lazily started fallback runtime and the connection becomes a pair of `futures` channels. The connection actor selects between inbound frames and commands so an idle receive cannot block a later close after cancellation. It restores undelivered frames and errors to the front of its queue when a receive is cancelled. Polling the socket also drives automatic pong replies; read-ahead is bounded to apply backpressure for slow consumers.

## License

MIT. See [LICENSE](LICENSE) and [source records](../../CREDITS.md).
