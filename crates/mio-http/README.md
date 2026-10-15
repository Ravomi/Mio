# mio-http

Mio source fork of Rig 0.44.0 (MIT, Copyright 2024 Playgrounds Analytics Inc.).
See [provenance and local changes](../../docs/rig-source.md).

The transport contracts every [Rig](https://github.com/0xPlaygrounds/rig)
provider sends through:

- `http_client`: `HttpClientExt`, the trait an HTTP transport implements,
  `DynHttpClient` (an erased client with middleware), request framing,
  multipart forms and the transport error type;
- `ws_client` (feature `websocket`): the websocket connection contract the
  Responses WebSocket session is written against;
- `wasm_compat`: the `Send`/`Sync` bounds and boxed futures that relax on
  browser wasm.

This crate has no transport of its own. `mio-reqwest` (HTTP) and
`mio-tungstenite` (websocket) are the bundled transports, and depend only on
this crate. `mio-core` re-exports these modules at `mio_core::http_client`,
`mio_core::ws_client` and `mio_core::wasm_compat`, so provider code and
users reach them there.

A transport author depends on `mio-http` alone and implements
`HttpClientExt`:

```rust
use mio_http::http_client::{HttpClientExt, DynHttpClient};

fn erase(client: impl HttpClientExt + Clone + 'static) -> DynHttpClient {
    DynHttpClient::new(client)
}
```

The `test-utils` feature adds HTTP client doubles for tests.
