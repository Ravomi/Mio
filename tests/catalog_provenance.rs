//! CLI/site capabilities must refer to the same checked-in core facts.
use mio_coding_agent::rpc::list_catalog_models;
use serde_json::Value;

#[test]
fn cli_and_generated_site_share_capabilities_and_provenance() {
    let site: Value = serde_json::from_str(include_str!("../docs/model-catalog.json")).unwrap();
    let entries = list_catalog_models(None);
    assert_eq!(
        entries.len(),
        site["totalModels"].as_u64().unwrap() as usize
    );
    for entry in entries {
        let provider = site["providers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|provider| provider["name"] == entry.provider)
            .unwrap();
        let model = provider["models"]
            .as_array()
            .unwrap()
            .iter()
            .find(|model| model["id"] == entry.model)
            .unwrap();
        assert_eq!(
            model["reasoning"], entry.supports_thinking,
            "{}/{}",
            entry.provider, entry.model
        );
        assert_eq!(model["capabilitySource"], entry.capability_source);
        assert_eq!(
            site["source"]["capabilitiesSha256"],
            entry.capability_snapshot_sha256
        );
        assert_eq!(entry.reachable, None);
        assert_eq!(entry.authenticated, None);
        assert_eq!(entry.protocol_supported, None);
        if entry.provider == "ollama" {
            assert_eq!(entry.credential_present, None);
        }
    }
}

#[test]
fn native_provider_presets_are_not_documented_as_compatibility_gateways() {
    let site: Value = serde_json::from_str(include_str!("../docs/model-catalog.json")).unwrap();
    for (name, protocol) in [
        ("anthropic", "Anthropic Messages"),
        ("google", "Gemini GenerateContent"),
        ("ollama", "Native Ollama /api/chat"),
    ] {
        let provider = site["providers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|provider| provider["name"] == name)
            .unwrap();
        assert_eq!(provider["protocol"], protocol);
        assert_eq!(provider["requiresGateway"], false);
    }
}
