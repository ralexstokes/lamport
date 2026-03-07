use std::time::Duration;

use reqwest::blocking::{Client, RequestBuilder};
use serde_json::{Value, json};

use super::provider::{ProviderAnswer, ProviderConfig, ProviderKind, ProviderRequest};

// Prompt-caching notes:
// - OpenAI caches exact prompt prefixes automatically, so the request keeps a
//   stable system prompt and append-only message history at the front, with the
//   newest turn update appended at the end. Reusing prompt_cache_key improves
//   routing for those shared prefixes.
// - Anthropic's Messages API supports automatic prompt caching with top-level
//   cache_control. Keeping the conversation append-only lets the cache point
//   advance with each turn instead of rebuilding a brand new prompt every time.
pub(super) fn request_provider(
    config: &ProviderConfig,
    request: ProviderRequest,
) -> Result<ProviderAnswer, String> {
    match config.kind {
        ProviderKind::OpenAI => request_openai(config, &request),
        ProviderKind::Anthropic => request_anthropic(config, &request),
    }
}

fn request_openai(
    config: &ProviderConfig,
    request: &ProviderRequest,
) -> Result<ProviderAnswer, String> {
    let client = build_client("OpenAI")?;
    let (status, body) = send_json_request(
        "OpenAI",
        client
            .post("https://api.openai.com/v1/responses")
            .bearer_auth(&config.api_key)
            .json(&json!({
                "model": config.model,
                "input": openai_messages(request),
                "max_output_tokens": 512,
                "prompt_cache_key": config.prompt_cache_key(),
                "reasoning": {
                    "effort": "minimal"
                },
                "text": {
                    "verbosity": "low"
                }
            })),
    )?;

    if !status.is_success() {
        return Err(format_http_error("OpenAI", status.as_u16(), &body));
    }

    if let Some(reason) = openai_incomplete_reason(&body) {
        return Err(format!(
            "OpenAI response was incomplete before producing visible output: {reason}. Body: {body}"
        ));
    }

    let text = extract_openai_text(&body)
        .ok_or_else(|| format!("OpenAI response did not contain text: {body}"))?;

    Ok(ProviderAnswer {
        model: config.model.clone(),
        text,
        cache_summary: openai_cache_summary(&body),
    })
}

fn request_anthropic(
    config: &ProviderConfig,
    request: &ProviderRequest,
) -> Result<ProviderAnswer, String> {
    let client = build_client("Anthropic")?;
    let (status, body) = send_json_request(
        "Anthropic",
        client
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", &config.api_key)
            .header("anthropic-version", "2023-06-01")
            .json(&json!({
                "model": config.model,
                "max_tokens": 128,
                "temperature": 0.0,
                "cache_control": {
                    "type": "ephemeral"
                },
                "system": &request.system_prompt,
                "messages": anthropic_messages(request),
                "output_config": {
                    "format": {
                        "type": "json_schema",
                        "schema": {
                            "type": "object",
                            "properties": {
                                "move": {
                                    "type": "integer",
                                    "enum": [1, 2, 3, 4, 5, 6, 7, 8, 9],
                                    "description": "The legal tic-tac-toe square to play."
                                }
                            },
                            "required": ["move"],
                            "additionalProperties": false
                        }
                    }
                }
            })),
    )?;

    if !status.is_success() {
        return Err(format_http_error("Anthropic", status.as_u16(), &body));
    }

    let text = extract_anthropic_text(&body)
        .ok_or_else(|| format!("Anthropic response did not contain text: {body}"))?;

    Ok(ProviderAnswer {
        model: config.model.clone(),
        text,
        cache_summary: anthropic_cache_summary(&body),
    })
}

fn build_client(provider: &str) -> Result<Client, String> {
    Client::builder()
        .timeout(Duration::from_secs(90))
        .build()
        .map_err(|error| format!("build {provider} client failed: {error}"))
}

fn send_json_request(
    provider: &str,
    request: RequestBuilder,
) -> Result<(reqwest::StatusCode, Value), String> {
    let response = request
        .send()
        .map_err(|error| format!("{provider} request failed: {error}"))?;
    let status = response.status();
    let body = response
        .json()
        .map_err(|error| format!("decode {provider} response failed: {error}"))?;
    Ok((status, body))
}

fn openai_messages(request: &ProviderRequest) -> Vec<Value> {
    let mut messages = Vec::with_capacity(request.messages.len() + 1);
    messages.push(json!({
        "role": "system",
        "content": &request.system_prompt,
    }));
    messages.extend(request.messages.iter().map(|message| {
        json!({
            "role": message.role.as_str(),
            "content": &message.content,
        })
    }));
    messages
}

fn anthropic_messages(request: &ProviderRequest) -> Vec<Value> {
    request
        .messages
        .iter()
        .map(|message| {
            json!({
                "role": message.role.as_str(),
                "content": &message.content,
            })
        })
        .collect()
}

fn extract_openai_text(body: &Value) -> Option<String> {
    if let Some(text) = body.get("output_text").and_then(Value::as_str) {
        let text = text.trim();
        if !text.is_empty() {
            return Some(text.to_owned());
        }
    }

    let mut parts = Vec::new();
    for item in body.get("output")?.as_array()? {
        let Some(content) = item.get("content").and_then(Value::as_array) else {
            continue;
        };

        for block in content {
            let text = block.get("text").and_then(Value::as_str);
            if let Some(text) = text.map(str::trim).filter(|text| !text.is_empty()) {
                parts.push(text.to_owned());
            }
        }
    }

    (!parts.is_empty()).then(|| parts.join("\n\n"))
}

fn openai_incomplete_reason(body: &Value) -> Option<&str> {
    if body.get("status").and_then(Value::as_str)? != "incomplete" {
        return None;
    }

    body.get("incomplete_details")
        .and_then(|details| details.get("reason"))
        .and_then(Value::as_str)
}

fn extract_anthropic_text(body: &Value) -> Option<String> {
    let mut parts = Vec::new();
    for block in body.get("content")?.as_array()? {
        let Some(text) = block.get("text").and_then(Value::as_str) else {
            continue;
        };

        let text = text.trim();
        if !text.is_empty() {
            parts.push(text.to_owned());
        }
    }

    (!parts.is_empty()).then(|| parts.join("\n\n"))
}

fn openai_cache_summary(body: &Value) -> Option<String> {
    let usage = body.get("usage")?;
    let input_tokens = usage
        .get("input_tokens")
        .or_else(|| usage.get("prompt_tokens"))
        .and_then(Value::as_u64);
    let cached_tokens = usage
        .get("input_tokens_details")
        .or_else(|| usage.get("prompt_tokens_details"))
        .and_then(|details| details.get("cached_tokens"))
        .and_then(Value::as_u64)
        .or_else(|| usage.get("cached_tokens").and_then(Value::as_u64));

    match (input_tokens, cached_tokens) {
        (Some(input_tokens), Some(cached_tokens)) => Some(format!(
            "{cached_tokens} of {input_tokens} input tokens served from cache",
        )),
        (None, Some(cached_tokens)) => {
            Some(format!("{cached_tokens} input tokens served from cache"))
        }
        (Some(input_tokens), None) => Some(format!(
            "{input_tokens} input tokens processed (cache usage not reported)",
        )),
        (None, None) => None,
    }
}

fn anthropic_cache_summary(body: &Value) -> Option<String> {
    let usage = body.get("usage")?;
    let input_tokens = usage.get("input_tokens").and_then(Value::as_u64);
    let cache_read = usage
        .get("cache_read_input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cache_write = usage
        .get("cache_creation_input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);

    let mut parts = Vec::new();
    if let Some(input_tokens) = input_tokens {
        parts.push(format!("{input_tokens} uncached input tokens"));
    }
    if cache_read > 0 {
        parts.push(format!("{cache_read} cache-read tokens"));
    }
    if cache_write > 0 {
        parts.push(format!("{cache_write} cache-write tokens"));
    }

    (!parts.is_empty()).then(|| parts.join(", "))
}

fn format_http_error(provider: &str, status: u16, body: &Value) -> String {
    if let Some(message) = body
        .get("error")
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
    {
        return format!("{provider} returned HTTP {status}: {message}");
    }

    if let Some(message) = body.get("message").and_then(Value::as_str) {
        return format!("{provider} returned HTTP {status}: {message}");
    }

    format!("{provider} returned HTTP {status}: {body}")
}
