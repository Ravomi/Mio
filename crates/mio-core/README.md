# mio-core

Mio model/provider contracts, messages, streaming and request preparation.
Build the local API reference with `cargo doc -p mio-core --no-deps`.

## Features
- Portable contracts for agent runtimes, including completions, messages, tools, and memory
- Full [GenAI Semantic Convention](https://opentelemetry.io/docs/specs/semconv/gen-ai/) compatibility
- 20+ model providers, all under one singular unified interface
- Built-in providers selectable as data: `providers::registry` names a vendor and a protocol family (`deepseek/openai:deepseek-chat`) or carries a whole typed configuration, and both round-trip through serde without a credential. Model references discard embedded credentials and reject empty identifiers; a configuration's `id()` returns a catalog selection only when its dialect name is registered. Providers outside this catalog can use a `Model` and `ModelAdapter` directly.
- Full support for LLM completion and embedding workflows
- Support for transcription, audio generation and image generation model capabilities
- Integrate LLMs in your app with minimal boilerplate
- Full WASM compatibility (core library only)

## Installation
```bash
cargo add mio-core --features reqwest
```

The `reqwest` feature gives provider clients a default HTTP transport, so
`OpenAI::from_env()` needs no client of yours. Without it, pass a transport
with `OpenAIConfig::from_env()?.connect(http)`.

## WASM target support

`mio-core` supports the browser-oriented `wasm32-unknown-unknown` target. When
the `pdf` feature is enabled, the host must provide the Web Crypto API's
`Crypto.getRandomValues` implementation, as modern browsers, Web Workers, and
Node.js 19 or later do. WASI targets are not supported.

## Simple example
```rust
use mio_core::{
    completion::CompletionRequest,
    providers::openai::{self, OpenAI},
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // The client reads `OPENAI_API_KEY` and sends through the bundled reqwest
    // client (mio-core's `reqwest` feature). `with_http` swaps the client.
    let model = OpenAI::from_env()?.completion(openai::GPT_5_2);

    let response = model
        .call(CompletionRequest::new("Who are you?").temperature(0.2))
        .await?;
    println!("{}", response.text());

    Ok(())
}
```
Note using `#[tokio::main]` requires you enable tokio's `macros` and `rt-multi-thread` features
or just `full` to enable all features (`cargo add tokio --features macros,rt-multi-thread`).

Provider implementations are maintained in `src/providers`; reusable agent
runtimes live in `mio-agent`, and coding tools and the CLI in `mio-coding-agent`.

## Provider selection persistence

Registry references distinguish registered presets from explicit configurations.
Qualified `vendor/format:model` strings remain unambiguous when a vendor gains
another protocol family, but do not freeze preset defaults, endpoint availability,
or model names. Explicit configurations retain host, route, and typed options and
serialize as objects rather than lossy labels. References remove credentials;
standalone configurations may hold them at runtime. Deserialization requires a
self-describing format such as JSON.

Copilot presets accept already-exchanged session tokens and derive their endpoint
from those tokens. Explicit configurations keep their host and instruction
placement. Token exchange and asynchronous SDK setup belong to the host. The
catalog is not exhaustive: other models can use `ModelAdapter` directly.
Registry-qualified names do not replace the provider names used in telemetry.

## Provider implementation

A provider has a serializable configuration (`OpenAIConfig`) and a client
(`OpenAI`), the configuration on a transport. The client builds one `Model`
per capability: the capability's wire, which says what to send and how to
read the reply, on the client's transport. One driver runs every model.
Chat-compatible dialects use the shared `Chat` wire and decoder with
`Dialect` data and `BodyRewrite` hooks rather than duplicating request conversion.
This keeps normalization, retry classification, and telemetry consistent.

An operation (`wire::Operation`) names its request, event and response types,
which event ends a reply, and the fold one reply's events pass through; it is
written once, and every provider of it is interchangeable behind a `DynModel`.
A crate outside Mio adds an operation and serves it with its own wire and
decoder over HTTP, or with `driver::Local` and a transport of its own for an
in-process runtime.

Each wire encodes requests without transport access and creates a fresh decoder
for each reply. Buffered and streaming replies use the same classifier and event
mapping. A buffered response with a distinct shape is another classified event,
not a separate normalization path. The driver owns framing and asynchronous I/O;
decoders can be exercised directly with frames without opening a connection.
Credentials stored in configuration use `Secret`, whose serialized redaction
must be replaced with a credential when the host reloads the configuration.

Construct normalized completion responses through their builders so finish
reasons reconcile with tool output. Preserve unknown terminal reasons in `Other`,
use the selected descriptor's provider name, and retain the decoded provider
payload in `raw` for typed inspection without a second request. Preserve error
bodies through `ProviderError::from_http_response` for failed HTTP responses and
`ProviderError::from_provider_body` for error envelopes on successful HTTP
responses.
Credentials require redacted debug output. Provider changes need coverage for
supported streaming, usage, tool and multimodal content, with examples and facade
exposure matching the configured capabilities.

Request serialization must use stable map ordering. Randomized `HashMap`
iteration can change request bytes and serialized schemas embedded in prompts,
reducing prefix-cache reuse. Compare raw serialized bytes when testing this;
canonicalized cassette JSON can hide ordering differences. The JSON helpers sort
map keys and recursively sort rendered values regardless of serde_json's
`preserve_order` feature.

## Anthropic prompt caching

Manual caching marks the system prompt, final tool definition, and final message
block. Automatic caching delegates the moving conversation breakpoint to the
provider. Combined mode retains static-prefix markers within the four-marker
budget, including any markers already supplied on provider-specific tools.

A longer TTL can keep shared tools and instructions cached across conversations
without paying the longer storage lifetime for every conversation tail. Set
`with_static_prefix_cache_ttl(CacheTtl::OneHour)` with automatic five-minute
caching for this arrangement. One-hour markers must precede five-minute markers;
request encoding rejects the inverse ordering. Cache-write prices differ by TTL,
so choose a lifetime based on actual reuse.

Caching is skipped when the prefix through a marker is below the model's minimum:

| Model | Minimum tokens |
| --- | ---: |
| `claude-opus-4-7`, `claude-opus-4-6`, `claude-opus-4-5` | 4096 |
| `claude-sonnet-4-6` | 2048 |
| `claude-sonnet-4-5`, `claude-opus-4-1`, `claude-opus-4`, `claude-sonnet-4` | 1024 |
| `claude-haiku-4-5` | 4096 |

## Gemini explicit caching

Explicit caching uploads reusable content and returns a `cachedContents` handle.
Requests can reuse it immediately, including across conversations. Storage is
billed per token-hour until deletion or expiry, in addition to cached-input
charges. Implicit prefix caching is automatic and best-effort; it does not
provide a handle or guarantee a warm first request.

A request using an explicit cache cannot also send `systemInstruction`, `tools`,
or `toolConfig`. Mio rejects these conflicts before sending. Agents therefore
need no preamble, advertised tools, or configured tool choice on those turns.
An empty `RequestPatch::active_tools` allow-list suppresses tool advertisements,
but does not make cached function declarations executable by the agent. Agent
advertisement and dispatch share a registry snapshot. To execute functions
specified in a cache, drive `GenerateContent` directly and append matching
function responses yourself. Provider-hosted tools such as `codeExecution`
need no caller-side dispatch.

Native structured output and context documents can accompany a cache because
neither adds those conflicting fields. `OutputMode::Tool` adds a synthetic tool
and extends the preamble; `OutputMode::Prompted` adds schema instructions to the
preamble. Neither can accompany a cache. Extractors select tool output mode.

Cache resource paths accept a bare id or a `cachedContents/` handle. Validation
rejects path separators, query delimiters, fragments, and traversal segments
rather than escaping them, so lookup, expiry updates, and deletion cannot be
retargeted by an invalid handle. A 403 or 404 on an existing handle maps to
`ProviderError::CacheExpired`, preserving the provider's reply because 403 can
also indicate credential or quota problems. Cache creation does not apply this
mapping, so authorization failures do not become recreation loops.

## Provider observations

`observe::AdapterContext` carries a caller-owned operation identity and a
`Witness` sink. Pass it separately from request data through
`Model::call_observed(request, context)` or `Model::stream_observed(request,
context)`; `Model::call` and `Model::stream` record nothing. Request builders
and request literals contain only provider request data.

Bus-backed `ModelHandle::call_observed` and `ModelHandle::stream_observed`
take the same context. `ModelAdapter` forwards `Dispatch::adapter_context`;
explicit caller context takes precedence over Recorder/Observe context for
that invocation, including through handler layers. It is never serialized into
provider data or effect records and is not inherited by child calls.

Clone the context for attempts of the same operation;
use a distinct non-sensitive identity for another logical call.
`for_host_attempt(subject, ordinal)` rebinds a host retry to its current
dispatch subject while sharing the logical operation and HTTP send counter.
Facts carry the optional host ordinal separately: multiple HTTP sends within
one dispatch share its host ordinal. Already-running attempts keep their
original subjects and ordinals. A changed logical request needs a new context.

The Gemini unary and SSE paths emit `Action::Adapter` facts for send, HTTP
response status, provider metadata, error envelopes and closure through their
shared drivers. Unary closure
reports decoding, error or drop. SSE closure distinguishes terminal, EOF,
partial frame, error and drop; recoverable corrupt frames are separate facts.
Usage facts retain provider-reported optional counts even when decoding or
normalization rejects a response. Each fact is a cumulative snapshot for its
attempt: replace earlier snapshots instead of summing them, keep absent counts
unknown, and do not add overlapping token categories to invent a total.
Provider verdicts retain reported finish/block reasons and model versions;
error-envelope fields remain separate from HTTP status and Mio retryability.
Sparse verdict fields update only when present. Response IDs and allowlisted
headers are bounded, scrubbed analysis data, excluded from semantic comparison.
An ID-only payload attaches its latest ID to the next verdict or attempt
closure without creating an empty semantic event. This also preserves the ID
on error and consumer drop. Known request credentials are redacted before
persistence; arbitrary response bodies and headers are not copied wholesale.
Credential extraction includes URL userinfo and known query keys, including
origin-form request URIs. Diagnostic comparisons account for percent escapes
and control removal without rewriting safe diagnostic text. Hosts can reuse
`observe::diagnostic_url_secrets` for endpoint configuration; its returned
secrets must stay in memory and must never be persisted or logged.
The normalized Gemini unary API closes after normalization, so an empty
decoded response closes as an error; the raw API retains its decode boundary.
Error closures also preserve the original typed error boundary before report
conversion erases HTTP subtypes: request construction, provider response,
response decoding, typed transport termination, or unknown. An opaque client
error stays unknown; its message is never used to infer a boundary. This field
does not change the error returned to the caller or its retryability.
`ObservationLog::with_clock` optionally stamps individual facts with host elapsed
time. Mio does not calculate run, handler or transport intervals. These stamps
describe the executing environment, including replay, and do not establish live
provider performance.
Partial-frame facts count raw bytes after the last blank SSE delimiter without
retaining their contents. Complete-frame counts include only frames passed to
the adapter driver, excluding provider-recognized analysis-only frames (Gemini
frames containing only a valid string `responseId`). Unknown, malformed and
otherwise-empty frames still count. Corrupt-frame ordinals start at one using
the same counting rule. `TransportEof`
records body completeness independently: a provider terminal followed by
an incomplete trailing frame retains both its terminal closure and partial
EOF evidence. The context does not promise cross-execution identity. The ECS completion adapter
forwards its driver's context when the invocation supplies no explicit context. Response bodies and
credentials are not copied into these boundary facts.

## License

MIT. See [LICENSE](LICENSE) and [source records](../../CREDITS.md).
