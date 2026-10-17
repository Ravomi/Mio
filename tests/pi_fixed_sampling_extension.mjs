/**
 * Offline test for evals/adapters/pi-fixed-sampling.ts.
 *
 * Loads the extension with Node's built-in TypeScript stripping (the extension only
 * uses erasable syntax), wires the `before_provider_request` hook into a fake Pi, and
 * checks that it pins temperature/max_tokens for the expected model and refuses
 * everything else. No provider is contacted and no credential is read.
 *
 * Run with: node tests/pi_fixed_sampling_extension.mjs
 */
import assert from "node:assert/strict";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const extensionPath = join(here, "..", "evals", "adapters", "pi-fixed-sampling.ts");

const module = await import(pathToFileURL(extensionPath).href);
assert.equal(typeof module.default, "function", "extension default export is a factory");

const handlers = new Map();
module.default({ on: (name, handler) => handlers.set(name, handler) });
const hook = handlers.get("before_provider_request");
assert.equal(typeof hook, "function", "extension registers before_provider_request");

const model = {
  provider: "deepseek",
  id: "deepseek-flash",
  api: "openai-completions",
  compat: { maxTokensField: "max_tokens" },
};

// The expected model is pinned, and the caller's object is not mutated on success.
const payload = { model: "deepseek-flash", messages: [], stream: true, temperature: 0.7 };
const pinned = hook({ type: "before_provider_request", payload }, { model });
assert.equal(pinned.temperature, 0, "temperature is forced to 0");
assert.equal(pinned.max_tokens, 8192, "output cap is forced to 8192");
assert.equal(pinned.model, "deepseek-flash");
assert.equal(pinned.messages, payload.messages);
assert.equal(payload.temperature, 0.7, "the input payload is not mutated on success");

function refuses(label, eventPayload, ctxModel) {
  assert.throws(
    () => hook({ type: "before_provider_request", payload: eventPayload }, { model: ctxModel }),
    /pi-fixed-sampling/,
    label,
  );
}

// A context/model that cannot be pinned must fail hard, never pass through.
assert.throws(
  () => hook({ type: "before_provider_request", payload: { model: "deepseek-flash" } },
              { model: undefined }),
  /pi-fixed-sampling/,
  "no active model",
);
refuses("wrong provider", { model: "deepseek-flash" }, { ...model, provider: "openai" });
refuses("wrong model id", { model: "deepseek-flash" }, { ...model, id: "deepseek-chat" });
refuses("wrong api", { model: "deepseek-flash" }, { ...model, api: "anthropic-messages" });
refuses("wrong output cap field", { model: "deepseek-flash" },
        { ...model, compat: { maxTokensField: "max_completion_tokens" } });
refuses("non-object payload", "not-a-request", model);
refuses("payload names another model", { model: "deepseek-v4-pro" }, model);
refuses("payload carries max_completion_tokens", { model: "deepseek-flash", max_completion_tokens: 10 },
        model);

// Refusals also poison the request so Pi cannot fall back to the old payload.
const poisoned = { model: "deepseek-flash" };
assert.throws(
  () => hook({ type: "before_provider_request", payload: poisoned }, { model: { ...model, id: "wrong" } }),
  /pi-fixed-sampling/,
);
assert.equal(poisoned.model, "pi-fixed-sampling-refused");

process.stdout.write("pi-fixed-sampling extension: ok\n");
