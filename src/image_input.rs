//! Bounded typed image input shared by SDK and RPC. The selected route must
//! have both a native or compatible image encoder and a catalogued image model.

use crate::{Config, Message, Provider, RunOptions};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use mio_core::{
    catalog::Catalog,
    message::{DocumentSourceKind, Image, ImageMediaType, UserContent},
    providers::registry::ProviderId,
};

const MAX_IMAGES: usize = 4;
const MAX_IMAGE_BYTES: usize = 512 * 1024;
const MAX_TOTAL_IMAGE_BYTES: usize = 1024 * 1024;
/// Deliberately high admission reserve per image. Provider image tokenization
/// depends on dimensions and detail, and actual usage is still authoritative.
pub(crate) const IMAGE_CONTEXT_RESERVE: usize = 32 * 1024;

pub fn validate_prompt(
    config: &Config,
    options: &RunOptions,
    prompt: &Message,
) -> Result<(), String> {
    let Message::User { content } = prompt else {
        return Err("Input must be a user message".into());
    };
    if content.is_empty() || content.len() > 8 {
        return Err("Input must contain 1–8 text or image blocks".into());
    }
    let mut text_len = 0usize;
    let mut images = 0usize;
    let mut image_bytes = 0usize;
    for item in content {
        match item {
            UserContent::Text(text) => text_len = text_len.saturating_add(text.text.len()),
            UserContent::Image(image) => {
                images += 1;
                if images > MAX_IMAGES {
                    return Err("Image input supports at most 4 images per turn".into());
                }
                image_bytes = image_bytes.saturating_add(validate_image(image)?);
                if image_bytes > MAX_TOTAL_IMAGE_BYTES {
                    return Err("Image input exceeds the 1 MiB total image limit".into());
                }
            }
            _ => return Err("Input supports only text and image blocks".into()),
        }
    }
    if text_len > 64 * 1024 || (images == 0 && !has_nonblank_text(content)) {
        return Err("输入需为 1–65536 字节".into());
    }
    if images > 0 {
        let Some((vendor, model)) = catalog_image_model(config, options) else {
            return Err("Image input is unavailable for this provider route".into());
        };
        if !model_supports_image(config, options) {
            return Err(format!(
                "Image input is unavailable for provider '{vendor}' model '{model}' in the local catalog"
            ));
        }
    }
    Ok(())
}

fn catalog_image_model<'a>(config: &Config, options: &'a RunOptions) -> Option<(&'a str, String)> {
    let vendor = match config.provider {
        Provider::Custom | Provider::CustomResponses => {
            options.catalog_provider.as_deref().unwrap_or("openai")
        }
        Provider::Deepseek => "deepseek",
        Provider::Anthropic => "anthropic",
        Provider::Google => "gcp.gemini",
        Provider::Ollama => "ollama",
        _ => return None,
    };
    let vendor = if matches!(vendor, "google" | "gemini") {
        "gcp.gemini"
    } else {
        vendor
    };
    let (_, model, _) = crate::parse_model_spec(&config.model);
    Some((vendor, model))
}

/// Local catalog admission for the configured route and model. This is a UI
/// hint; the provider can still reject the image or have a smaller context.
pub fn model_supports_image(config: &Config, options: &RunOptions) -> bool {
    let Some((vendor, model)) = catalog_image_model(config, options) else {
        return false;
    };
    ProviderId::catalog(vendor)
        .and_then(|id| Catalog::builtin().find(id, &model))
        .is_some_and(|spec| spec.input.image)
}

fn has_nonblank_text(content: &[UserContent]) -> bool {
    content
        .iter()
        .any(|item| matches!(item, UserContent::Text(text) if !text.text.trim().is_empty()))
}

/// Validate a local, typed image and return its decoded byte count. URLs and
/// provider file IDs are refused: they cannot be bounded or replayed here.
pub(crate) fn validate_image(image: &Image) -> Result<usize, String> {
    if image.native.is_some() {
        return Err("Image input cannot carry provider-native metadata".into());
    }
    let DocumentSourceKind::Base64(encoded) = &image.data else {
        return Err("Image input requires inline base64 data".into());
    };
    if encoded.is_empty() || encoded.len() > (MAX_IMAGE_BYTES * 4 / 3 + 4) {
        return Err("Image input exceeds the 512 KiB per-image limit".into());
    }
    let bytes = STANDARD
        .decode(encoded)
        .map_err(|_| "Image input contains invalid base64".to_string())?;
    if bytes.is_empty() || bytes.len() > MAX_IMAGE_BYTES {
        return Err("Image input exceeds the 512 KiB per-image limit".into());
    }
    let valid_type = match image.media_type.as_ref() {
        Some(ImageMediaType::PNG) => bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
        Some(ImageMediaType::JPEG) => bytes.starts_with(b"\xff\xd8\xff"),
        Some(ImageMediaType::GIF) => bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a"),
        Some(ImageMediaType::WEBP) => {
            bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP"
        }
        _ => false,
    };
    if !valid_type {
        return Err("Image input MIME type is unsupported or does not match the data".into());
    }
    Ok(bytes.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mio_core::message::Text;

    const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+XcZUAAAAASUVORK5CYII=";

    fn prompt(image: Image) -> Message {
        Message::User {
            content: vec![
                UserContent::Text(Text::new("describe")),
                UserContent::Image(image),
            ],
        }
    }

    fn image() -> Image {
        Image {
            data: DocumentSourceKind::Base64(PNG.into()),
            media_type: Some(ImageMediaType::PNG),
            detail: None,
            native: None,
        }
    }

    #[test]
    fn image_admission_checks_model_data_and_local_source() {
        let config = Config {
            provider: Provider::Custom,
            model: "gpt-4o".into(),
            ..Config::default()
        };
        validate_prompt(&config, &RunOptions::default(), &prompt(image())).unwrap();
        assert!(model_supports_image(&config, &RunOptions::default()));

        let mut wrong_type = image();
        wrong_type.media_type = Some(ImageMediaType::JPEG);
        assert!(validate_prompt(&config, &RunOptions::default(), &prompt(wrong_type)).is_err());

        let mut remote = image();
        remote.data = DocumentSourceKind::Url("https://example.invalid/image.png".into());
        assert!(validate_prompt(&config, &RunOptions::default(), &prompt(remote)).is_err());

        let unlisted = Config {
            model: "unlisted-model".into(),
            ..config
        };
        assert!(validate_prompt(&unlisted, &RunOptions::default(), &prompt(image())).is_err());
        assert!(!model_supports_image(&unlisted, &RunOptions::default()));

        let cost = crate::context_budget::message_cost(&prompt(image())).unwrap();
        assert!(cost >= IMAGE_CONTEXT_RESERVE);
        assert!(cost < IMAGE_CONTEXT_RESERVE + 1024);
    }

    #[test]
    fn native_routes_require_a_catalogued_image_model() {
        for (provider, model) in [
            (Provider::Anthropic, "claude-sonnet-4-5"),
            (Provider::Google, "gemini-2.5-pro"),
            (Provider::Ollama, "gemma3"),
        ] {
            let config = Config {
                provider,
                model: model.into(),
                ..Config::default()
            };
            validate_prompt(&config, &RunOptions::default(), &prompt(image())).unwrap();
            assert!(model_supports_image(&config, &RunOptions::default()));
        }

        let text_only = Config {
            provider: Provider::Ollama,
            model: "deepseek-r1".into(),
            ..Config::default()
        };
        assert!(validate_prompt(&text_only, &RunOptions::default(), &prompt(image())).is_err());
        assert!(!model_supports_image(&text_only, &RunOptions::default()));
    }
}
