//! Explicit local authorization import. Never executes an external runtime or edits its files.
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    fs::File,
    io::Read,
    path::{Path, PathBuf},
};

pub(crate) const PROFILE_FILE: &str = "antigravity-profile.json";
#[derive(Serialize, Deserialize)]
pub(crate) struct Profile {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: u64,
    pub project: String,
    pub client_id: String,
    pub client_secret: String,
    pub model: String,
    pub thinking: Option<String>,
}
#[derive(Debug, Clone, Serialize)]
pub struct Summary {
    pub model: String,
    pub thinking: Option<String>,
}

pub fn config_directory() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("MIO_CONFIG_DIR") {
        if path.is_empty() {
            return Err("MIO_CONFIG_DIR must not be empty".into());
        }
        return Ok(path.into());
    }
    if let Some(path) = std::env::var_os("XDG_CONFIG_HOME").filter(|p| !p.is_empty()) {
        return Ok(PathBuf::from(path).join("mio"));
    }
    std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join(".config/mio"))
        .ok_or("Set MIO_CONFIG_DIR for local account storage".into())
}
fn read(path: &Path, limit: usize) -> Result<Vec<u8>, String> {
    let file = File::open(path).map_err(|_| "Cannot read local authorization or settings file")?;
    if !file
        .metadata()
        .map_err(|_| "Cannot inspect local file")?
        .is_file()
    {
        return Err("Expected a regular local file".into());
    }
    let mut bytes = Vec::new();
    file.take((limit + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| "Cannot read local file")?;
    if bytes.len() > limit {
        return Err("Local authorization/settings file exceeds size limit".into());
    }
    Ok(bytes)
}
pub(crate) fn load(directory: &Path) -> Result<Profile, String> {
    let profile: Profile =
        serde_json::from_slice(&read(&directory.join(PROFILE_FILE), 1024 * 1024)?)
            .map_err(|_| "Invalid Mio Antigravity profile; import authorization again")?;
    validate(&profile)?;
    Ok(profile)
}
pub fn summary(directory: &Path) -> Result<Option<Summary>, String> {
    if !directory
        .join(PROFILE_FILE)
        .try_exists()
        .map_err(|_| "Cannot inspect Mio profile")?
    {
        return Ok(None);
    }
    let p = load(directory)?;
    Ok(Some(Summary {
        model: p.model,
        thinking: p.thinking,
    }))
}
fn field(value: &Value, key: &str) -> Result<String, String> {
    value[key]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .map(str::to_owned)
        .ok_or_else(|| format!("Authorization is missing a valid {key} field"))
}
fn validate(p: &Profile) -> Result<(), String> {
    if [
        &p.access_token,
        &p.refresh_token,
        &p.project,
        &p.client_id,
        &p.client_secret,
        &p.model,
    ]
    .iter()
    .any(|s| s.trim().is_empty() || s.len() > 65536)
        || p.expires_at == 0
    {
        return Err("Antigravity profile contains missing or invalid fields".into());
    }
    if p.thinking.as_deref().is_some_and(|level| {
        !["off", "minimal", "low", "medium", "high", "xhigh", "max"].contains(&level)
    }) {
        return Err("Unsupported Antigravity thinking level".into());
    }
    Ok(())
}
/// Decode known literal metadata from the installed provider without executing JavaScript.
fn oauth_literal(source: &str, name: &str) -> Result<String, String> {
    let expression = source
        .split_once(&format!("var {name} = "))
        .map(|(_, s)| s.split(';').next().unwrap_or_default())
        .ok_or("Installed provider has an unsupported OAuth metadata format")?;
    if let Some((_, rest)) = expression.split_once("Buffer.from(") {
        let literal = rest
            .trim_start()
            .strip_prefix('"')
            .and_then(|s| s.split('"').next())
            .ok_or("Invalid OAuth metadata literal")?;
        let decoded = STANDARD
            .decode(literal)
            .map_err(|_| "Invalid OAuth metadata encoding")?;
        return String::from_utf8(decoded).map_err(|_| "Invalid OAuth metadata text".into());
    }
    serde_json::from_str(expression.trim()).map_err(|_| {
        "Unsupported OAuth metadata; set MIO_GOOGLE_CLIENT_ID and MIO_GOOGLE_CLIENT_SECRET".into()
    })
}
/// Copy existing local Antigravity authorization and default model into one private native profile.
/// Source credentials are not changed. OAuth application metadata is local configuration, not bundled.
pub fn import_antigravity(source: &Path, destination: &Path) -> Result<Summary, String> {
    let auth: Value = serde_json::from_slice(&read(&source.join("auth.json"), 1024 * 1024)?)
        .map_err(|_| "Invalid source auth JSON")?;
    let settings: Value =
        serde_json::from_slice(&read(&source.join("settings.json"), 1024 * 1024)?)
            .map_err(|_| "Invalid source settings JSON")?;
    if settings["defaultProvider"] != "antigravity" || auth["antigravity"]["type"] != "oauth" {
        return Err("Source must select Antigravity and contain OAuth authorization".into());
    }
    let credential = &auth["antigravity"];
    let access: Value = serde_json::from_str(&field(credential, "access")?)
        .map_err(|_| "Invalid Antigravity access payload")?;
    let (client_id, client_secret) = match (
        std::env::var("MIO_GOOGLE_CLIENT_ID").or_else(|_| std::env::var("ANTIGRAVITY_CLIENT_ID")),
        std::env::var("MIO_GOOGLE_CLIENT_SECRET")
            .or_else(|_| std::env::var("ANTIGRAVITY_CLIENT_SECRET")),
    ) {
        (Ok(id), Ok(secret)) => (id, secret),
        (Err(_), Err(_)) => {
            let js = String::from_utf8(read(
                &source.join("npm/node_modules/pi-provider-antigravity/dist/index.js"),
                8 * 1024 * 1024,
            )?)
            .map_err(|_| "Invalid installed provider metadata")?;
            (
                oauth_literal(&js, "CLIENT_ID")?,
                oauth_literal(&js, "CLIENT_SECRET")?,
            )
        }
        _ => return Err("Set both MIO_GOOGLE_CLIENT_ID and MIO_GOOGLE_CLIENT_SECRET".into()),
    };
    let profile = Profile {
        access_token: field(&access, "token")?,
        refresh_token: field(credential, "refresh")?,
        expires_at: credential["expires"]
            .as_u64()
            .ok_or("Invalid authorization expiry")?
            / 1000,
        project: field(&access, "projectId")?,
        client_id,
        client_secret,
        model: field(&settings, "defaultModel")?,
        thinking: settings["defaultThinkingLevel"].as_str().map(str::to_owned),
    };
    validate(&profile)?;
    std::fs::create_dir_all(destination)
        .map_err(|_| "Cannot create Mio configuration directory")?;
    let bytes = serde_json::to_vec(&profile).map_err(|_| "Cannot encode imported authorization")?;
    crate::accounts::save_private(&destination.join(PROFILE_FILE), &bytes)
        .map_err(|_| "Cannot save Mio authorization")?;
    Ok(Summary {
        model: profile.model,
        thinking: profile.thinking,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn metadata_is_decoded_without_execution() {
        assert_eq!(oauth_literal("var CLIENT_ID = process.env.OVERRIDE || Buffer.from(\n\"dGVzdC1pZA==\", \"base64\").toString(\"utf8\");", "CLIENT_ID").unwrap(), "test-id");
        assert!(oauth_literal("var CLIENT_ID = dangerous();", "CLIENT_ID").is_err());
    }
    #[test]
    fn imports_one_private_profile_without_modifying_source() {
        let source = tempfile::tempdir().unwrap();
        let destination = tempfile::tempdir().unwrap();
        let provider = source
            .path()
            .join("npm/node_modules/pi-provider-antigravity/dist");
        std::fs::create_dir_all(&provider).unwrap();
        std::fs::write(
            provider.join("index.js"),
            "var CLIENT_ID = \"test-client\"; var CLIENT_SECRET = \"test-secret\";",
        )
        .unwrap();
        let auth = serde_json::json!({"antigravity":{"type":"oauth","refresh":"private-refresh","access":serde_json::json!({"token":"private-access","projectId":"test-project"}).to_string(),"expires":2000000000000u64},"another":{"key":"preserved"}}).to_string();
        std::fs::write(source.path().join("auth.json"), &auth).unwrap();
        std::fs::write(source.path().join("settings.json"), r#"{"defaultProvider":"antigravity","defaultModel":"gemini-test","defaultThinkingLevel":"high"}"#).unwrap();
        let summary = import_antigravity(source.path(), destination.path()).unwrap();
        let loaded = load(destination.path()).unwrap();
        assert_eq!(loaded.expires_at, 2000000000);
        assert_eq!(loaded.access_token, "private-access");
        assert_eq!(summary.model, "gemini-test");
        assert!(!serde_json::to_string(&summary).unwrap().contains("private"));
        assert_eq!(
            std::fs::read_to_string(source.path().join("auth.json")).unwrap(),
            auth
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(destination.path().join(PROFILE_FILE))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        std::fs::write(source.path().join("auth.json"), "sensitive-invalid-json").unwrap();
        assert!(
            !import_antigravity(source.path(), destination.path())
                .unwrap_err()
                .contains("sensitive-invalid-json")
        );
        assert_eq!(
            load(destination.path()).unwrap().access_token,
            "private-access"
        );
    }
}
