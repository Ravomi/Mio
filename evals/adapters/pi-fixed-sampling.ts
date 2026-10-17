/**
 * Test-only Pi extension: pin sampling for the cross-agent comparison runner.
 *
 * Pi has no `--temperature` CLI flag, so `evals/configs/pi-deepseek-comparison.json`
 * loads this extension explicitly:
 *
 *   pi --no-extensions -e {repo}/evals/adapters/pi-fixed-sampling.ts ...
 *
 * `--no-extensions` disables discovery (the user's extensions and Pi's built-ins)
 * while still loading the explicit `-e` path, so this file is the only extension in
 * the run. It reads no settings, environment or credentials of its own: only the
 * `DEEPSEEK_API_KEY` that the provider configuration already expects.
 *
 * The `before_provider_request` hook receives the final provider request after Pi
 * has assembled it, and its return value replaces that payload. This extension uses
 * it to force the DeepSeek Chat Completions sampling fields:
 *
 *   temperature: 0, max_tokens: 8192
 *
 * Before changing anything it verifies `ctx.model.provider` / `ctx.model.id` /
 * `ctx.model.api` / the output-cap field name. Anything else is refused instead of
 * silently ignored.
 *
 * Refusals are deliberately double-signalled. Pi catches handler exceptions and
 * keeps the previous payload, so a bare `throw` would let the request proceed
 * unchanged. `refuse()` therefore also overwrites `payload.model` with an invalid
 * sentinel id, which makes DeepSeek reject the request, and throws so Pi records an
 * extension error as well.
 */
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

const EXPECTED_PROVIDER = "deepseek";
const EXPECTED_MODEL = "deepseek-flash";
const EXPECTED_API = "openai-completions";
const EXPECTED_MAX_TOKENS_FIELD = "max_tokens";
const FIXED_TEMPERATURE = 0;
const FIXED_MAX_OUTPUT_TOKENS = 8192;
/** An id no provider serves, used to poison a request that cannot be pinned. */
const REFUSAL_MODEL = "pi-fixed-sampling-refused";

interface PayloadShape {
  model?: unknown;
  max_completion_tokens?: unknown;
  [key: string]: unknown;
}

/** Refuse the request without ever letting it proceed silently. */
function refuse(payload: unknown, reason: string): never {
  const detail = `pi-fixed-sampling: refusing to send this request: ${reason}`;
  try {
    process.stderr.write(`${detail}\n`);
  } catch {
    // stderr may be closed; the thrown error below still records the failure.
  }
  // Pi keeps the previous payload when a handler throws, so poison the model id
  // as well: the provider then rejects this request outright.
  if (payload !== null && typeof payload === "object") {
    try {
      (payload as PayloadShape).model = REFUSAL_MODEL;
    } catch {
      // A frozen payload cannot be poisoned; the thrown error is the only signal.
    }
  }
  throw new Error(detail);
}

export default function (pi: ExtensionAPI): void {
  pi.on("before_provider_request", (event, ctx) => {
    const model = ctx.model;
    if (model === undefined) {
      return refuse(event.payload, "no model is active in this context");
    }
    if (model.provider !== EXPECTED_PROVIDER) {
      return refuse(
        event.payload,
        `provider ${JSON.stringify(model.provider)} is not ${EXPECTED_PROVIDER}`,
      );
    }
    if (model.id !== EXPECTED_MODEL) {
      return refuse(
        event.payload,
        `model ${JSON.stringify(model.id)} is not ${EXPECTED_MODEL}`,
      );
    }
    if (model.api !== EXPECTED_API) {
      return refuse(event.payload, `api ${JSON.stringify(model.api)} is not ${EXPECTED_API}`);
    }
    const compat = (model as unknown as { compat?: { maxTokensField?: unknown } }).compat;
    if (compat?.maxTokensField !== EXPECTED_MAX_TOKENS_FIELD) {
      return refuse(
        event.payload,
        `output cap field is ${JSON.stringify(compat?.maxTokensField)} for this api, not ${EXPECTED_MAX_TOKENS_FIELD}`,
      );
    }
    const payload = event.payload;
    if (payload === null || typeof payload !== "object" || Array.isArray(payload)) {
      return refuse(payload, "the provider payload is not a request object");
    }
    const body = payload as PayloadShape;
    if (body.model !== EXPECTED_MODEL) {
      return refuse(
        payload,
        `the payload names model ${JSON.stringify(body.model)}, not ${EXPECTED_MODEL}`,
      );
    }
    if (body.max_completion_tokens !== undefined) {
      return refuse(
        payload,
        "the payload already carries max_completion_tokens for a different api",
      );
    }
    return { ...body, temperature: FIXED_TEMPERATURE, max_tokens: FIXED_MAX_OUTPUT_TOKENS };
  });
}
