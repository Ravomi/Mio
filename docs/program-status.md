# OSC 7501 Program Status Reporting in Mio

Mio implements the [OSC 7501 Program Status Protocol](https://www.superlogical.com/rex/docs/build/program-status) (rev 0.4). Mio reports its own runtime state; terminal emulators decide how to display it.

## Format

```text
\x1b]7501;state=<state>[:kind=<kind>]:app=mio[:msg=<base64>]\x1b\
```

- **States**: `working`, `blocked`, `done`, `idle`, `error`.
- **Kind**: Mio's `Blocked` encoder includes a typed kind: `permission`, `question`, or `auth`. The protocol itself allows the kind to be omitted.
- **App**: Fixed `:app=mio`.
- **Msg**: Base64-encoded UTF-8 string with control characters sanitized.
- **Constraints**:
  - Message raw UTF-8 input is bounded to 2048 bytes (truncated on char boundary).
  - Total sequence output is bounded to 4096 bytes.

## CLI Behavior

- Status sequences are emitted to `stderr` only when running in text mode and `stderr.is_terminal()` is true.
- JSON mode (`--json`) and RPC mode (`--mode rpc`) disable OSC 7501 emission.
- Sequence emission is deduplicated: repeated identical state and message payloads are not re-emitted.
- Lifecycle transitions:
  - Startup in interactive text mode: reports `idle`.
  - Submitting the next input: reports `idle`. Waiting at the prompt does not overwrite the previous `done` or `error` report.
  - Task execution: reports `working`.
  - Tool execution / text generation: reports `working` (with tool info in `msg`).
  - Approval prompts (`--approve`): reports `blocked` with `kind=permission`, restoring `working` once answered.
  - Successful turn completion: reports `done`.
  - Turn error or startup configuration error: reports `error`.
  - User cancellation (Ctrl-C during turn): reports `idle`.
  - Command line `--help` and `--version` do not emit status reports.

The library exposes `program_status::format_osc7501` for embedding hosts without writing to process output. The CLI currently reports permission waits; it does not infer questions or authentication waits from model text.

On Unix, run the PTY regression suite after `cargo build --locked --bin mio` with `python3 tests/osc7501.py`. It uses local mock HTTP responses and temporary workspaces.
