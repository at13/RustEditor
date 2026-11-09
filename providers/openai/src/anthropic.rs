//! Anthropic Messages API translation (editor provider protocol stays the same).
use re_agent::protocol::{Event, ModelRequest, WireMessage};
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub fn body(request: &ModelRequest, model: Option<&str>) -> Value {
    let mut messages: Vec<Value> = Vec::new();
    for message in &request.messages {
        let (role, content) = match message {
            WireMessage::User { content } => ("user", vec![json!({"type":"text","text":content})]),
            WireMessage::Assistant {
                content,
                tool_calls,
            } => {
                let mut parts = Vec::new();
                if !content.is_empty() {
                    parts.push(json!({"type":"text","text":content}));
                }
                parts.extend(tool_calls.iter().map(
                    |c| json!({"type":"tool_use","id":c.id,"name":c.name,"input":c.arguments}),
                ));
                ("assistant", parts)
            }
            WireMessage::Tool {
                tool_call_id,
                content,
            } => (
                "user",
                vec![json!({"type":"tool_result","tool_use_id":tool_call_id,"content":content})],
            ),
        };
        if let Some(last) = messages.last_mut().filter(|m| m["role"] == role) {
            last["content"].as_array_mut().unwrap().extend(content);
        } else {
            messages.push(json!({"role":role,"content":content}));
        }
    }
    let mut body = json!({"model":model.unwrap_or(&request.model), "system":request.system,
        "messages":messages,"max_tokens":request.max_output_tokens,"stream":true});
    if !request.tools.is_empty() {
        body["tools"] = request.tools.iter().map(|t| json!({"name":t.name,"description":t.description,"input_schema":t.input_schema})).collect();
    }
    body
}

#[derive(Default)]
pub struct Stream {
    calls: BTreeMap<u64, (String, String, String, Value)>,
    pub done: bool,
    reason: String,
    input: u64,
    output: u64,
}
impl Stream {
    pub fn line(&mut self, line: &str) -> Vec<Event> {
        let Some(data) = line.strip_prefix("data:") else {
            return vec![];
        };
        let v: Value = match serde_json::from_str(data.trim()) {
            Ok(v) => v,
            Err(_) => {
                return vec![Event::Error {
                    message: "invalid Anthropic response chunk".into(),
                }];
            }
        };
        match v["type"].as_str().unwrap_or_default() {
            "message_start" => {
                self.input = v["message"]["usage"]["input_tokens"].as_u64().unwrap_or(0);
                vec![]
            }
            "content_block_start" if v["content_block"]["type"] == "tool_use" => {
                let c = &v["content_block"];
                self.calls.insert(
                    v["index"].as_u64().unwrap_or(0),
                    (
                        c["id"].as_str().unwrap_or_default().into(),
                        c["name"].as_str().unwrap_or_default().into(),
                        String::new(),
                        c["input"].clone(),
                    ),
                );
                vec![]
            }
            "content_block_delta" => {
                let delta = &v["delta"];
                if let Some(text) = delta["text"].as_str() {
                    return vec![Event::Text { delta: text.into() }];
                }
                if let Some(args) = delta["partial_json"].as_str()
                    && let Some(call) = self.calls.get_mut(&v["index"].as_u64().unwrap_or(0))
                {
                    call.2.push_str(args);
                }
                vec![]
            }
            "message_delta" => {
                self.reason = v["delta"]["stop_reason"]
                    .as_str()
                    .unwrap_or("end_turn")
                    .into();
                self.output = v["usage"]["output_tokens"].as_u64().unwrap_or(0);
                vec![]
            }
            "message_stop" => {
                self.done = true;
                let mut events = Vec::new();
                for (_, (id, name, args, input)) in std::mem::take(&mut self.calls) {
                    let arguments = if args.is_empty() {
                        input
                    } else {
                        match serde_json::from_str(&args) {
                            Ok(v) => v,
                            Err(_) => {
                                return vec![Event::Error {
                                    message: "Anthropic returned invalid tool arguments".into(),
                                }];
                            }
                        }
                    };
                    events.push(Event::ToolCall {
                        id,
                        name,
                        arguments,
                    });
                }
                events.push(Event::Usage {
                    input_tokens: self.input,
                    output_tokens: self.output,
                });
                events.push(Event::Done {
                    reason: match self.reason.as_str() {
                        "tool_use" => "tool_calls",
                        "max_tokens" => "length",
                        _ => "stop",
                    }
                    .into(),
                });
                events
            }
            "error" => vec![Event::Error {
                message: v["error"]["message"]
                    .as_str()
                    .unwrap_or("Anthropic error")
                    .into(),
            }],
            _ => vec![],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tool_results_are_grouped_and_streamed_calls_are_assembled() {
        let mut r = ModelRequest::new("sys", &[], &[]);
        r.messages = vec![
            WireMessage::Tool {
                tool_call_id: "a".into(),
                content: "one".into(),
            },
            WireMessage::Tool {
                tool_call_id: "b".into(),
                content: "two".into(),
            },
        ];
        assert_eq!(
            body(&r, Some("claude"))["messages"][0]["content"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        let mut s = Stream::default();
        s.line(r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"c","name":"read_buffer","input":{}}}"#);
        s.line(r#"data: {"type":"content_block_delta","index":1,"delta":{"partial_json":"{\"start_line\":2}"}}"#);
        s.line(r#"data: {"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":4}}"#);
        let events = s.line(r#"data: {"type":"message_stop"}"#);
        assert!(
            matches!(&events[0],Event::ToolCall {arguments,..} if arguments["start_line"] == 2)
        );
        assert!(matches!(&events[2],Event::Done {reason} if reason == "tool_calls"));
    }
}
