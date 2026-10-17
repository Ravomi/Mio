#!/usr/bin/env python3
"""
Generate docs/model-catalog.json directly from src/rpc.rs PROVIDER_PRESETS.
Supports --check to ensure catalog is in sync with Rust source code.
Does not contact external services or read user credentials.
"""

import json
import hashlib
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
RPC_RS = ROOT / "src" / "rpc.rs"
CATALOG_JSON = ROOT / "docs" / "model-catalog.json"
CORE_CATALOG = ROOT / "crates/mio-core/src/catalog/models.json"

PROVIDER_METADATA = {
    "antigravity": {
        "displayName": "Antigravity (Google Cloud Code PA)",
        "protocol": "Google Cloud Code PA (OAuth Profile)",
        "notesZh": "凭据查找优先级：MIO_CONFIG_DIR > XDG_CONFIG_HOME/mio > ~/.config/mio；支持通过 mio --import-antigravity ~/.pi/agent 导入已有凭据。已有 profile 可提供默认 model。实际可用模型与档位取决于账号权限；Antigravity 档位由该适配器与账号权限决定，不代替其他 Provider 的原生协议与 catalog 校验。",
        "notesEn": "Credential search priority: MIO_CONFIG_DIR > XDG_CONFIG_HOME/mio > ~/.config/mio; supports importing existing profiles via mio --import-antigravity ~/.pi/agent. An existing profile can provide a default model. Available models and levels depend on account entitlements; Antigravity levels depend on this adapter and account entitlements; other providers use their own native protocol and catalog validation.",
        "requiresGateway": False,
    },
    "openai": {
        "displayName": "OpenAI",
        "protocol": "OpenAI Chat Completions",
        "notesZh": "OpenAI Chat Completions 接口；工具与思考能力优先读取底层 catalog，具体档位由模型验证，不代表账号或端点已实测。",
        "notesEn": "OpenAI Chat Completions; model capabilities come from the core catalog and local validation, not a live account or endpoint probe.",
        "requiresGateway": False,
    },
    "anthropic": {
        "displayName": "Anthropic",
        "protocol": "Anthropic Messages",
        "notesZh": "使用原生 Anthropic Messages 协议。若接入 OpenAI 兼容网关，应显式选择 custom。目录声明不代表账号、认证或线上协议已验证。",
        "notesEn": "Uses native Anthropic Messages. Select custom for an OpenAI-compatible gateway. Catalog facts do not prove account access, authentication or live protocol support.",
        "requiresGateway": False,
    },
    "google": {
        "displayName": "Google Gemini",
        "protocol": "Gemini GenerateContent",
        "notesZh": "使用 Google 原生 GenerateContent 协议。凭据来自 GEMINI_API_KEY 或 GOOGLE_API_KEY；账号可用性须另行实测。",
        "notesEn": "Uses native Google GenerateContent. Credentials come from GEMINI_API_KEY or GOOGLE_API_KEY; account readiness requires a separate live probe.",
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
        "protocol": "Native Ollama /api/chat",
        "notesZh": "使用本地 Ollama 原生 /api/chat 协议。预设不要求远程密钥，不代表本地守护进程正在运行。",
        "notesEn": "Uses the local native Ollama /api/chat protocol. No remote key is required by the preset; daemon reachability is not inferred.",
        "requiresGateway": False,
    },
}

def parse_presets_from_rpc():
    content = RPC_RS.read_text(encoding="utf-8")
    core = json.loads(CORE_CATALOG.read_text(encoding="utf-8"))
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
            model_id = m.group(1)
            vendor = "gcp.gemini" if name == "google" else name
            row = core.get(vendor, {}).get("models", {}).get(model_id)
            models.append({
                "id": model_id,
                "reasoning": bool(row.get("reasoning", False)) if row is not None else m.group(2) == "true",
                "capabilitySource": "mio-core/catalog/models.json" if row is not None else "provider-preset (not live verified)",
                "contextWindow": row.get("limit", {}).get("context") if row is not None else None,
                "maxOutputTokens": row.get("limit", {}).get("output") if row is not None else None,
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
        "version": "1.1",
        "source": {
            "presets": "src/rpc.rs::PROVIDER_PRESETS",
            "capabilities": "crates/mio-core/src/catalog/models.json",
            "capabilitiesSha256": hashlib.sha256(CORE_CATALOG.read_bytes()).hexdigest(),
            "reviewedAt": "2026-10-15",
            "reviewScope": "local source alignment; not live provider verification",
        },
        "liveVerification": {"reachable": None, "authenticated": None, "protocolSupported": None},
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
