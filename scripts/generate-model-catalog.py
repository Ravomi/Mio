#!/usr/bin/env python3
"""
Generate docs/model-catalog.json directly from src/rpc.rs PROVIDER_PRESETS.
Supports --check to ensure catalog is in sync with Rust source code.
Does not contact external services or read user credentials.
"""

import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
RPC_RS = ROOT / "src" / "rpc.rs"
CATALOG_JSON = ROOT / "docs" / "model-catalog.json"

PROVIDER_METADATA = {
    "antigravity": {
        "displayName": "Antigravity (Google Cloud Code PA)",
        "protocol": "Google Cloud Code PA (OAuth Profile)",
        "notesZh": "凭据查找优先级：MIO_CONFIG_DIR > XDG_CONFIG_HOME/mio > ~/.config/mio；支持通过 mio --import-antigravity ~/.pi/agent 导入已有凭据。已有 profile 可提供默认 model。实际可用模型与档位取决于账号权限；一般 thinking 仅在 Antigravity 适配器中消费生效，不声称完整支持。",
        "notesEn": "Credential search priority: MIO_CONFIG_DIR > XDG_CONFIG_HOME/mio > ~/.config/mio; supports importing existing profiles via mio --import-antigravity ~/.pi/agent. An existing profile can provide a default model. Available models and levels depend on account entitlements; generic thinking is consumed only by the Antigravity adapter without claiming universal support.",
        "requiresGateway": False,
    },
    "openai": {
        "displayName": "OpenAI",
        "protocol": "OpenAI Chat Completions",
        "notesZh": "OpenAI 官方 Chat Completions 接口，原生支持函数调用与流式事件；思考档位由 Antigravity 专享，OpenAI 驱动忽略通用 thinking 预算。",
        "notesEn": "Official OpenAI Chat Completions API with native tool-calling; thinking levels are Antigravity-only and ignored by standard OpenAI driver.",
        "requiresGateway": False,
    },
    "anthropic": {
        "displayName": "Anthropic (Compatible Gateway Required)",
        "protocol": "OpenAI Chat Completions (Via Gateway)",
        "notesZh": "⚠️ Mio 采用通用 OpenAI Chat 驱动。原生 api.anthropic.com 端点格式不兼容，必须搭配 OpenAI 兼容网关并显式指定 --base-url https://your-gateway.example/v1。",
        "notesEn": "⚠️ Mio uses a standard OpenAI Chat driver. Native api.anthropic.com endpoint is incompatible; you must provide an OpenAI-compatible gateway via explicit --base-url.",
        "requiresGateway": True,
    },
    "google": {
        "displayName": "Google Gemini (OpenAI Endpoint)",
        "protocol": "Gemini OpenAI-Compatible Endpoint",
        "notesZh": "使用 Google Generative Language 提供的 OpenAI 兼容接口 (/v1beta/openai)。支持通过 GEMINI_API_KEY 或 GOOGLE_API_KEY 接入。",
        "notesEn": "Uses Google Generative Language OpenAI-compatible endpoint (/v1beta/openai). Configured via GEMINI_API_KEY or GOOGLE_API_KEY.",
        "requiresGateway": False,
    },
    "deepseek": {
        "displayName": "DeepSeek",
        "protocol": "DeepSeek API (OpenAI Compatible)",
        "notesZh": "DeepSeek 官方 API 兼容 OpenAI 格式。支持 deepseek-chat 及内置推理模型 deepseek-reasoner。",
        "notesEn": "Official DeepSeek API (OpenAI-compatible). Supports deepseek-chat and deepseek-reasoner.",
        "requiresGateway": False,
    },
    "openrouter": {
        "displayName": "OpenRouter",
        "protocol": "OpenRouter API (OpenAI Compatible)",
        "notesZh": "多模型聚合网关，统一使用 openrouter.ai/api/v1 端点接入各类上游模型。",
        "notesEn": "Multi-provider aggregation gateway routed through openrouter.ai/api/v1.",
        "requiresGateway": False,
    },
    "groq": {
        "displayName": "Groq",
        "protocol": "Groq OpenAI-Compatible API",
        "notesZh": "Groq 超高速 LPU 推理端点，兼容 OpenAI 格式协议。",
        "notesEn": "Groq ultra-fast LPU inference endpoint, OpenAI-compatible.",
        "requiresGateway": False,
    },
    "xai": {
        "displayName": "xAI (Grok)",
        "protocol": "xAI API (OpenAI Compatible)",
        "notesZh": "xAI 官方 Grok API 接口，完全兼容 OpenAI 协议。",
        "notesEn": "xAI official Grok API endpoint, fully OpenAI-compatible.",
        "requiresGateway": False,
    },
    "mistral": {
        "displayName": "Mistral AI",
        "protocol": "Mistral API (OpenAI Compatible)",
        "notesZh": "Mistral AI 官方端点，支持 Codestral 与 Mistral Large。",
        "notesEn": "Mistral AI official endpoint, supports Codestral and Mistral Large.",
        "requiresGateway": False,
    },
    "ollama": {
        "displayName": "Ollama (Local)",
        "protocol": "Local Ollama (OpenAI Compatible)",
        "notesZh": "本地运行的 Ollama 实例（默认 http://127.0.0.1:11434/v1），无需外部密钥即可离线使用。",
        "notesEn": "Locally running Ollama instance (default http://127.0.0.1:11434/v1), completely offline without remote keys.",
        "requiresGateway": False,
    },
}

def parse_presets_from_rpc():
    content = RPC_RS.read_text(encoding="utf-8")
    match = re.search(r"const PROVIDER_PRESETS: &\[ProviderPreset\] = &\[(.*?)\];", content, re.DOTALL)
    if not match:
        raise RuntimeError("Failed to locate PROVIDER_PRESETS in src/rpc.rs")
    block = match.group(1)

    preset_blocks = re.findall(r"ProviderPreset\s*\{(.*?)\},", block, re.DOTALL)
    presets = []

    for pb in preset_blocks:
        name_match = re.search(r'name:\s*"([^"]+)"', pb)
        base_url_match = re.search(r'base_url:\s*"([^"]+)"', pb)
        env_vars_match = re.search(r'env_vars:\s*&\[(.*?)\]', pb, re.DOTALL)
        models_match = re.search(r'models:\s*&\[(.*?)\]', pb, re.DOTALL)

        if not (name_match and base_url_match and env_vars_match and models_match):
            continue

        name = name_match.group(1)
        base_url = base_url_match.group(1)
        env_vars = [m.group(1) for m in re.finditer(r'"([^"]+)"', env_vars_match.group(1))]
        
        models = []
        for m in re.finditer(r'\(\s*"([^"]+)"\s*,\s*(true|false)\s*\)', models_match.group(1)):
            models.append({
                "id": m.group(1),
                "reasoning": m.group(2) == "true",
            })

        meta = PROVIDER_METADATA.get(name, {
            "displayName": name.capitalize(),
            "protocol": "OpenAI Compatible",
            "notesZh": f"{name} 提供商预设。",
            "notesEn": f"Preset for {name}.",
            "requiresGateway": False,
        })

        presets.append({
            "name": name,
            "displayName": meta["displayName"],
            "protocol": meta["protocol"],
            "baseUrl": base_url,
            "envVars": env_vars,
            "requiresGateway": meta["requiresGateway"],
            "notesZh": meta["notesZh"],
            "notesEn": meta["notesEn"],
            "models": models,
        })

    return presets

def generate_catalog():
    presets = parse_presets_from_rpc()
    catalog = {
        "version": "1.0",
        "description": "Static snapshot of Mio built-in provider presets from src/rpc.rs PROVIDER_PRESETS. Does not read user credentials.",
        "totalProviders": len(presets),
        "totalModels": sum(len(p["models"]) for p in presets),
        "providers": presets,
    }
    return json.dumps(catalog, indent=2, ensure_ascii=False) + "\n"

def main():
    check_mode = "--check" in sys.argv
    new_json = generate_catalog()

    if check_mode:
        if not CATALOG_JSON.exists():
            print(f"FAIL: {CATALOG_JSON} does not exist. Run scripts/generate-model-catalog.py to create it.", file=sys.stderr)
            sys.exit(1)
        current = CATALOG_JSON.read_text(encoding="utf-8")
        if current != new_json:
            print(f"FAIL: {CATALOG_JSON} is out of sync with src/rpc.rs. Run scripts/generate-model-catalog.py to regenerate.", file=sys.stderr)
            sys.exit(1)
        print("OK: docs/model-catalog.json is in sync with src/rpc.rs")
        sys.exit(0)

    CATALOG_JSON.write_text(new_json, encoding="utf-8")
    print(f"Generated {CATALOG_JSON}")

if __name__ == "__main__":
    main()
