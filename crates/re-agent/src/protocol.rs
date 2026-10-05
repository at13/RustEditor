//! Provider protocol: one JSON request on stdin, JSON-lines events on
//! stdout (see docs/design-proposal.md, 3.13.8).

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    API_VERSION,
    conversation::{Message, ToolCall},
    tools::ToolSpec,
};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum WireMessage {
    User {
        content: String,
    },
    Assistant {
        content: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<ToolCall>,
    },
    Tool {
        tool_call_id: String,
        content: String,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WireTool {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelRequest {
    pub api: u64,
    pub model: String,
    pub system: String,
    pub messages: Vec<WireMessage>,
    pub tools: Vec<WireTool>,
    pub max_output_tokens: u32,
}

impl ModelRequest {
    pub fn new(system: &str, messages: &[Message], tools: &[ToolSpec]) -> Self {
        Self {
            api: API_VERSION,
            model: String::new(),
            system: system.to_owned(),
            messages: wire_messages(messages),
            tools: tools
                .iter()
                .map(|t| WireTool {
                    name: t.name.clone(),
                    description: t.description.clone(),
                    input_schema: t.input_schema.clone(),
                })
                .collect(),
            max_output_tokens: 4096,
        }
    }
}

/// Most characters of one message sent to the model (tool output is also
/// capped by each tool; this protects against huge stored results).
const MAX_MESSAGE_CHARS: usize = 200_000;

fn cap(text: &str) -> String {
    if text.len() <= MAX_MESSAGE_CHARS {
        return text.to_owned();
    }
    let mut end = MAX_MESSAGE_CHARS;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[... truncated ...]", &text[..end])
}

/// Conversation messages as sent to the model (notices are dropped).
pub fn wire_messages(messages: &[Message]) -> Vec<WireMessage> {
    messages
        .iter()
        .filter_map(|m| match m {
            Message::User { text } => Some(WireMessage::User { content: cap(text) }),
            Message::Assistant { text, tool_calls } => Some(WireMessage::Assistant {
                content: cap(text),
                tool_calls: tool_calls.clone(),
            }),
            Message::Tool {
                call_id,
                content,
                is_error,
                ..
            } => Some(WireMessage::Tool {
                tool_call_id: call_id.clone(),
                content: if *is_error {
                    format!("ERROR: {}", cap(content))
                } else {
                    cap(content)
                },
            }),
            Message::Notice { .. } => None,
        })
        .collect()
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    Text {
        delta: String,
    },
    ToolCall {
        id: String,
        name: String,
        #[serde(default)]
        arguments: Value,
    },
    Usage {
        #[serde(default)]
        input_tokens: u64,
        #[serde(default)]
        output_tokens: u64,
    },
    Done {
        #[serde(default)]
        reason: String,
    },
    Error {
        message: String,
    },
}

/// Parses one stdout line; blank lines are skipped, bad lines are errors.
pub fn parse_event(line: &str) -> Option<Event> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    Some(serde_json::from_str(line).unwrap_or_else(|e| Event::Error {
        message: format!(
            "provider sent an invalid line ({e}): {}",
            line.chars().take(200).collect::<String>()
        ),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn requests_serialize_to_the_documented_shape() {
        let messages = vec![
            Message::User { text: "hi".into() },
            Message::Notice {
                text: "not sent".into(),
            },
            Message::Assistant {
                text: "".into(),
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "read_buffer".into(),
                    arguments: json!({}),
                }],
            },
            Message::Tool {
                call_id: "c1".into(),
                name: "read_buffer".into(),
                content: "boom".into(),
                is_error: true,
            },
        ];
        let request = ModelRequest::new("sys", &messages, &[]);
        let value = serde_json::to_value(&request).unwrap();
        assert_eq!(value["api"], 0);
        assert_eq!(value["messages"].as_array().unwrap().len(), 3);
        assert_eq!(
            value["messages"][0],
            json!({"role": "user", "content": "hi"})
        );
        assert_eq!(value["messages"][1]["tool_calls"][0]["name"], "read_buffer");
        assert_eq!(
            value["messages"][2],
            json!({"role": "tool", "tool_call_id": "c1", "content": "ERROR: boom"})
        );
    }

    #[test]
    fn events_parse_from_json_lines() {
        assert_eq!(
            parse_event(r#"{"type":"text","delta":"Hi"}"#),
            Some(Event::Text { delta: "Hi".into() })
        );
        assert_eq!(
            parse_event(r#"{"type":"tool_call","id":"c","name":"t","arguments":{"a":1}}"#),
            Some(Event::ToolCall {
                id: "c".into(),
                name: "t".into(),
                arguments: json!({"a": 1})
            })
        );
        assert_eq!(parse_event("  "), None);
        assert!(matches!(parse_event("nonsense"), Some(Event::Error { .. })));
        assert!(cap(&"é".repeat(MAX_MESSAGE_CHARS)).ends_with("[... truncated ...]"));
    }
}
