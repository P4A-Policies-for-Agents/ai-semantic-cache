// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Salesforce, Inc.

use crate::key::CanonicalPrompt;
use serde_json::Value;

#[derive(Debug, Clone)]
pub struct Extracted {
    pub prompt: CanonicalPrompt,
    pub model: String,
    pub stream: bool,
}

#[derive(Debug)]
pub enum ExtractOutcome {
    Ok(Extracted),
    /// Body is structurally valid but has no extractable prompt — forward as-is, no cache.
    Skip(&'static str),
}

#[derive(Debug, Clone)]
pub enum PromptExtractor {
    Openai,
    Anthropic,
    Cohere,
    Mistral,
    Bedrock,
    Vertex,
    // NOTE: a `Dataweave` variant existed here briefly. It was wired
    // into the schema and `build_extractor` but always returned `Skip`
    // from this enum's `extract()` — meaning operators who set
    // `promptExtractor: dataweave` got every request silently treated
    // as `skip-unparseable`. Removed pending real DataWeave evaluation
    // in the variant binaries (architecture.md §13 v2 backlog).
}

impl PromptExtractor {
    pub fn extract(&self, body: &Value) -> ExtractOutcome {
        match self {
            PromptExtractor::Openai => extract_openai(body),
            PromptExtractor::Anthropic => extract_anthropic(body),
            PromptExtractor::Cohere => extract_cohere(body),
            PromptExtractor::Mistral => extract_mistral(body),
            PromptExtractor::Bedrock => extract_bedrock(body),
            PromptExtractor::Vertex => extract_vertex(body),
        }
    }
}

fn extract_openai(body: &Value) -> ExtractOutcome {
    let model = match body.get("model").and_then(|v| v.as_str()) {
        Some(m) => m.to_string(),
        None => return ExtractOutcome::Skip("openai: missing `model`"),
    };
    let stream = body
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let messages = match body.get("messages").and_then(|v| v.as_array()) {
        Some(m) if !m.is_empty() => m,
        _ => return ExtractOutcome::Skip("openai: empty or missing `messages`"),
    };
    let pairs: Vec<(&str, &str)> = messages
        .iter()
        .filter_map(|m| {
            let role = m.get("role").and_then(|v| v.as_str())?;
            let content = m.get("content").and_then(|v| v.as_str())?;
            Some((role, content))
        })
        .collect();
    if pairs.is_empty() {
        return ExtractOutcome::Skip("openai: no string-content messages");
    }
    ExtractOutcome::Ok(Extracted {
        prompt: CanonicalPrompt::from_messages(&pairs),
        model,
        stream,
    })
}

fn extract_anthropic(body: &Value) -> ExtractOutcome {
    let model = match body.get("model").and_then(|v| v.as_str()) {
        Some(m) => m.to_string(),
        None => return ExtractOutcome::Skip("anthropic: missing `model`"),
    };
    let stream = body
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let messages = match body.get("messages").and_then(|v| v.as_array()) {
        Some(m) if !m.is_empty() => m,
        _ => return ExtractOutcome::Skip("anthropic: empty or missing `messages`"),
    };
    let pairs: Vec<(&str, &str)> = messages
        .iter()
        .filter_map(|m| {
            let role = m.get("role").and_then(|v| v.as_str())?;
            let content = m.get("content").and_then(|v| v.as_str())?;
            Some((role, content))
        })
        .collect();
    if pairs.is_empty() {
        return ExtractOutcome::Skip("anthropic: no string-content messages");
    }
    ExtractOutcome::Ok(Extracted {
        prompt: CanonicalPrompt::from_messages(&pairs),
        model,
        stream,
    })
}

fn extract_cohere(body: &Value) -> ExtractOutcome {
    let model = match body.get("model").and_then(|v| v.as_str()) {
        Some(m) => m.to_string(),
        None => return ExtractOutcome::Skip("cohere: missing `model`"),
    };
    let stream = body
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let message = match body.get("message").and_then(|v| v.as_str()) {
        Some(m) => m,
        None => return ExtractOutcome::Skip("cohere: missing `message`"),
    };
    ExtractOutcome::Ok(Extracted {
        prompt: CanonicalPrompt::from_text(message),
        model,
        stream,
    })
}

fn extract_mistral(body: &Value) -> ExtractOutcome {
    // Mistral chat is OpenAI-compatible.
    extract_openai(body)
}

fn extract_bedrock(body: &Value) -> ExtractOutcome {
    let model = body
        .get("modelId")
        .and_then(|v| v.as_str())
        .or_else(|| body.get("model").and_then(|v| v.as_str()))
        .map(|s| s.to_string());
    let model = match model {
        Some(m) => m,
        None => return ExtractOutcome::Skip("bedrock: missing `modelId`"),
    };
    let stream = false; // Bedrock streaming is endpoint-selected, not a body field.
    if let Some(messages) = body.get("messages").and_then(|v| v.as_array()) {
        let pairs: Vec<(&str, &str)> = messages
            .iter()
            .filter_map(|m| {
                let role = m.get("role").and_then(|v| v.as_str())?;
                let content = m.get("content").and_then(|v| v.as_str()).or_else(|| {
                    m.get("content")
                        .and_then(|v| v.as_array())
                        .and_then(|a| a.first())
                        .and_then(|el| el.get("text").and_then(|v| v.as_str()))
                })?;
                Some((role, content))
            })
            .collect();
        if !pairs.is_empty() {
            return ExtractOutcome::Ok(Extracted {
                prompt: CanonicalPrompt::from_messages(&pairs),
                model,
                stream,
            });
        }
    }
    if let Some(prompt) = body.get("inputText").and_then(|v| v.as_str()) {
        return ExtractOutcome::Ok(Extracted {
            prompt: CanonicalPrompt::from_text(prompt),
            model,
            stream,
        });
    }
    if let Some(prompt) = body.get("prompt").and_then(|v| v.as_str()) {
        return ExtractOutcome::Ok(Extracted {
            prompt: CanonicalPrompt::from_text(prompt),
            model,
            stream,
        });
    }
    ExtractOutcome::Skip("bedrock: no extractable prompt field")
}

fn extract_vertex(body: &Value) -> ExtractOutcome {
    let model = match body.get("model").and_then(|v| v.as_str()) {
        Some(m) => m.to_string(),
        None => return ExtractOutcome::Skip("vertex: missing `model`"),
    };
    let stream = false;
    let contents = match body.get("contents").and_then(|v| v.as_array()) {
        Some(c) if !c.is_empty() => c,
        _ => return ExtractOutcome::Skip("vertex: missing `contents`"),
    };
    let pairs: Vec<(&str, &str)> = contents
        .iter()
        .filter_map(|c| {
            let role = c.get("role").and_then(|v| v.as_str()).unwrap_or("user");
            let parts = c.get("parts").and_then(|v| v.as_array())?;
            let text = parts
                .iter()
                .find_map(|p| p.get("text").and_then(|v| v.as_str()))?;
            Some((role, text))
        })
        .collect();
    if pairs.is_empty() {
        return ExtractOutcome::Skip("vertex: no text parts");
    }
    ExtractOutcome::Ok(Extracted {
        prompt: CanonicalPrompt::from_messages(&pairs),
        model,
        stream,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ok(o: ExtractOutcome) -> Extracted {
        match o {
            ExtractOutcome::Ok(e) => e,
            _ => panic!("expected Ok"),
        }
    }

    fn is_skip(o: &ExtractOutcome) -> bool {
        matches!(o, ExtractOutcome::Skip(_))
    }

    #[test]
    fn openai_single_message() {
        let body = json!({"model":"gpt-4o","messages":[{"role":"user","content":"hi"}]});
        let e = ok(PromptExtractor::Openai.extract(&body));
        assert_eq!(e.model, "gpt-4o");
        assert!(!e.stream);
    }

    #[test]
    fn openai_stream_flag() {
        let body =
            json!({"model":"gpt-4o","messages":[{"role":"user","content":"hi"}],"stream":true});
        let e = ok(PromptExtractor::Openai.extract(&body));
        assert!(e.stream);
    }

    #[test]
    fn openai_missing_messages_skipped() {
        let body = json!({"model":"gpt-4o"});
        assert!(is_skip(&PromptExtractor::Openai.extract(&body)));
    }

    #[test]
    fn openai_empty_messages_skipped() {
        let body = json!({"model":"gpt-4o","messages":[]});
        assert!(is_skip(&PromptExtractor::Openai.extract(&body)));
    }

    #[test]
    fn anthropic_extracts_messages_and_model() {
        let body = json!({"model":"claude-3-5","messages":[{"role":"user","content":"hi"}]});
        let e = ok(PromptExtractor::Anthropic.extract(&body));
        assert_eq!(e.model, "claude-3-5");
    }

    #[test]
    fn cohere_extracts_message_field() {
        let body = json!({"model":"command-r","message":"hi"});
        let e = ok(PromptExtractor::Cohere.extract(&body));
        assert_eq!(e.model, "command-r");
    }

    #[test]
    fn mistral_uses_openai_shape() {
        let body = json!({"model":"mistral-large","messages":[{"role":"user","content":"hi"}]});
        let e = ok(PromptExtractor::Mistral.extract(&body));
        assert_eq!(e.model, "mistral-large");
    }

    #[test]
    fn bedrock_invoke_model_inputtext() {
        let body = json!({"modelId":"amazon.titan-text","inputText":"hi"});
        let e = ok(PromptExtractor::Bedrock.extract(&body));
        assert_eq!(e.model, "amazon.titan-text");
    }

    #[test]
    fn bedrock_converse_messages_array_content() {
        let body = json!({
            "modelId":"anthropic.claude-3-5",
            "messages":[{"role":"user","content":[{"text":"hi"}]}]
        });
        let e = ok(PromptExtractor::Bedrock.extract(&body));
        assert_eq!(e.model, "anthropic.claude-3-5");
    }

    #[test]
    fn vertex_extracts_text_part() {
        let body = json!({
            "model":"gemini-1.5",
            "contents":[{"role":"user","parts":[{"text":"hi"}]}]
        });
        let e = ok(PromptExtractor::Vertex.extract(&body));
        assert_eq!(e.model, "gemini-1.5");
    }

    #[test]
    fn vertex_missing_contents_skipped() {
        let body = json!({"model":"gemini-1.5"});
        assert!(is_skip(&PromptExtractor::Vertex.extract(&body)));
    }
}
