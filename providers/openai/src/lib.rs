//! RustEditor provider helper: OpenAI-compatible chat completion APIs,
//! Anthropic Messages API, and a bridge to the GitHub Copilot SDK provider.
//!
//! Reads one `re_agent::protocol::ModelRequest` on stdin, calls
//! `<base-url>/chat/completions` with streaming, and writes
//! `re_agent::protocol::Event`s as JSON lines on stdout.
//!
//! ```text
//! rusteditor-openai [--backend openai|anthropic|copilot] [--credential ID]
//!                   [--base-url URL] [--api-key-env NAME] [--auth-header NAME]
//!                   [--model NAME] [--max-tokens-field NAME] [--no-usage]
//! ```
//!
//! API keys are read from the OS credential store (`--credential`) or the
//! environment (`OPENAI_API_KEY` / `ANTHROPIC_API_KEY`), never from the
//! protocol request. Without either option, unauthenticated local servers
//! such as Ollama still work. Copilot uses the CLI's own authentication.

use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader, Read, Write},
    time::Duration,
};

use re_agent::protocol::{Event, ModelRequest, WireMessage};
use serde_json::{Value, json};

mod anthropic;
mod copilot;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Options {
    pub backend: String,
    pub credential: Option<String>,
    pub copilot_sdk: Option<String>,
    pub check_auth: bool,
    pub base_url: String,
    pub api_key_env: String,
    pub auth_header: String,
    pub model: Option<String>,
    pub max_tokens_field: String,
    pub usage: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            backend: "openai".into(),
            credential: None,
            copilot_sdk: None,
            check_auth: false,
            base_url: "https://api.openai.com/v1".into(),
            api_key_env: "OPENAI_API_KEY".into(),
            auth_header: "Authorization".into(),
            model: None,
            max_tokens_field: "max_tokens".into(),
            usage: true,
        }
    }
}

pub fn parse_args(args: &[String]) -> Result<Options, String> {
    let mut options = Options::default();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = || {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{arg} needs a value"))
        };
        match arg.as_str() {
            "--backend" => options.backend = value()?,
            "--credential" => options.credential = Some(value()?),
            "--copilot-sdk" => options.copilot_sdk = Some(value()?),
            "--check-auth" => options.check_auth = true,
            "--base-url" => options.base_url = value()?.trim_end_matches('/').to_owned(),
            "--api-key-env" => options.api_key_env = value()?,
            "--auth-header" => options.auth_header = value()?,
            "--model" => options.model = Some(value()?),
            "--max-tokens-field" => options.max_tokens_field = value()?,
            "--no-usage" => options.usage = false,
            other => return Err(format!("unknown argument {other}")),
        }
    }
    if !matches!(options.backend.as_str(), "openai" | "anthropic" | "copilot") {
        return Err("unknown provider backend".into());
    }
    if options.backend == "anthropic" {
        if options.base_url == "https://api.openai.com/v1" {
            options.base_url = "https://api.anthropic.com/v1".into();
        }
        if options.api_key_env == "OPENAI_API_KEY" {
            options.api_key_env = "ANTHROPIC_API_KEY".into();
        }
        options.auth_header = "x-api-key".into();
    }
    Ok(options)
}

/// The chat completions request body.
pub fn body(request: &ModelRequest, options: &Options) -> Value {
    let mut messages = vec![json!({"role": "system", "content": request.system})];
    for message in &request.messages {
        messages.push(match message {
            WireMessage::User { content } => json!({"role": "user", "content": content}),
            WireMessage::Assistant {
                content,
                tool_calls,
            } => {
                let mut m = json!({"role": "assistant", "content": content});
                if !tool_calls.is_empty() {
                    m["tool_calls"] = tool_calls
                        .iter()
                        .map(|c| {
                            json!({
                                "id": c.id,
                                "type": "function",
                                "function": {"name": c.name, "arguments": c.arguments.to_string()}
                            })
                        })
                        .collect();
                }
                m
            }
            WireMessage::Tool {
                tool_call_id,
                content,
            } => json!({"role": "tool", "tool_call_id": tool_call_id, "content": content}),
        });
    }
    let model = options
        .model
        .clone()
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| request.model.clone());
    let mut body = json!({"model": model, "messages": messages, "stream": true});
    body[options.max_tokens_field.as_str()] = json!(request.max_output_tokens);
    if options.usage {
        body["stream_options"] = json!({"include_usage": true});
    }
    if !request.tools.is_empty() {
        body["tools"] = request
            .tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "function": {"name": t.name, "description": t.description, "parameters": t.input_schema}
                })
            })
            .collect();
    }
    body
}

#[derive(Default)]
struct PendingCall {
    id: String,
    name: String,
    arguments: String,
}

/// Turns streamed chunks into events.
#[derive(Default)]
pub struct Stream {
    calls: BTreeMap<u64, PendingCall>,
    finish: Option<String>,
    done: bool,
}

impl Stream {
    /// Handles one SSE line; returns events to emit now.
    pub fn line(&mut self, line: &str) -> Vec<Event> {
        let Some(data) = line.strip_prefix("data:") else {
            return Vec::new();
        };
        let data = data.trim();
        if data == "[DONE]" {
            return self.finish();
        }
        let chunk: Value = match serde_json::from_str(data) {
            Ok(chunk) => chunk,
            Err(e) => {
                return vec![Event::Error {
                    message: format!("bad response chunk: {e}"),
                }];
            }
        };
        let mut events = Vec::new();
        if let Some(error) = chunk.get("error") {
            events.push(Event::Error {
                message: error_message(error),
            });
            self.done = true;
            return events;
        }
        if let Some(usage) = chunk.get("usage").filter(|u| !u.is_null()) {
            events.push(Event::Usage {
                input_tokens: usage["prompt_tokens"].as_u64().unwrap_or(0),
                output_tokens: usage["completion_tokens"].as_u64().unwrap_or(0),
            });
        }
        for choice in chunk["choices"].as_array().into_iter().flatten() {
            let delta = &choice["delta"];
            if let Some(text) = delta["content"].as_str().filter(|t| !t.is_empty()) {
                events.push(Event::Text { delta: text.into() });
            }
            for (i, call) in delta["tool_calls"]
                .as_array()
                .into_iter()
                .flatten()
                .enumerate()
            {
                let index = call["index"].as_u64().unwrap_or(i as u64);
                let pending = self.calls.entry(index).or_default();
                if let Some(id) = call["id"].as_str() {
                    pending.id.push_str(id);
                }
                if let Some(name) = call["function"]["name"].as_str() {
                    pending.name.push_str(name);
                }
                if let Some(args) = call["function"]["arguments"].as_str() {
                    pending.arguments.push_str(args);
                }
            }
            if let Some(reason) = choice["finish_reason"].as_str() {
                self.finish = Some(reason.to_owned());
            }
        }
        events
    }

    /// Emits pending tool calls and `Done` (once).
    pub fn finish(&mut self) -> Vec<Event> {
        if self.done {
            return Vec::new();
        }
        self.done = true;
        let mut events = Vec::new();
        for (index, call) in std::mem::take(&mut self.calls) {
            let arguments = if call.arguments.trim().is_empty() {
                json!({})
            } else {
                serde_json::from_str(&call.arguments)
                    .unwrap_or_else(|_| json!({"_invalid_json": call.arguments}))
            };
            events.push(Event::ToolCall {
                id: if call.id.is_empty() {
                    format!("call_{index}")
                } else {
                    call.id
                },
                name: call.name,
                arguments,
            });
        }
        let reason = match self.finish.as_deref() {
            Some("length") => "length",
            _ if !events.is_empty() => "tool_calls",
            _ => "stop",
        };
        events.push(Event::Done {
            reason: reason.into(),
        });
        events
    }
}

fn error_message(error: &Value) -> String {
    error["message"]
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| error.to_string())
}

fn emit(out: &mut impl Write, event: &Event) -> bool {
    let line = serde_json::to_string(event).expect("events serialize");
    writeln!(out, "{line}").and_then(|()| out.flush()).is_ok()
}

/// Runs one request. Returns the process exit code.
pub fn run(args: &[String], mut input: impl Read, mut out: impl Write) -> i32 {
    let fail = |out: &mut dyn Write, message: String| {
        let line = serde_json::to_string(&Event::Error { message }).expect("serialize");
        let _ = writeln!(out, "{line}");
        1
    };
    let options = match parse_args(args) {
        Ok(options) => options,
        Err(e) => return fail(&mut out, e),
    };
    if options.check_auth {
        if options.backend != "copilot" {
            return fail(
                &mut out,
                "Auth check is only supported for Copilot SDK".into(),
            );
        }
        return match copilot::run(
            None,
            options.copilot_sdk.as_deref().map(std::path::Path::new),
            true,
            &mut out,
        ) {
            Ok(()) => 0,
            Err(e) => fail(&mut out, e),
        };
    }
    let mut text = String::new();
    if let Err(e) = input.read_to_string(&mut text) {
        return fail(&mut out, format!("cannot read the request: {e}"));
    }
    let request: ModelRequest = match serde_json::from_str(&text) {
        Ok(request) => request,
        Err(e) => return fail(&mut out, format!("invalid request: {e}")),
    };
    if request.api != re_agent::API_VERSION {
        return fail(&mut out, "unsupported provider protocol version".into());
    }
    if options.backend == "copilot" {
        return match copilot::run(
            Some(&request),
            options.copilot_sdk.as_deref().map(std::path::Path::new),
            false,
            &mut out,
        ) {
            Ok(()) => 0,
            Err(e) => fail(&mut out, e),
        };
    }
    let anthropic = options.backend == "anthropic";
    let url = format!(
        "{}/{}",
        options.base_url,
        if anthropic {
            "messages"
        } else {
            "chat/completions"
        }
    );
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(20))
        .timeout_read(Duration::from_secs(180))
        .build();
    let mut call = agent.post(&url).set("Content-Type", "application/json");
    if anthropic {
        call = call.set("anthropic-version", "2023-06-01");
    }
    let key = match options.credential.as_deref() {
        Some(id) => match re_platform::credentials::get(id) {
            Ok(key) => key.or_else(|| std::env::var(&options.api_key_env).ok()),
            Err(_) => match std::env::var(&options.api_key_env) {
                Ok(key) if !key.is_empty() => Some(key),
                _ => return fail(&mut out, "Cannot access the OS credential store. Update the key in provider settings or use the API-key environment variable.".into()),
            },
        },
        None => std::env::var(&options.api_key_env).ok(),
    };
    if options.credential.is_some() && key.as_ref().is_none_or(String::is_empty) {
        return fail(&mut out, "No API key. Add one in Agent > Providers.".into());
    }
    if let Some(key) = key.as_deref().filter(|k| !k.is_empty()) {
        call = if options.auth_header.eq_ignore_ascii_case("authorization") {
            call.set("Authorization", &format!("Bearer {key}"))
        } else {
            call.set(&options.auth_header, key)
        };
    }
    let request_body = if anthropic {
        anthropic::body(&request, options.model.as_deref())
    } else {
        body(&request, &options)
    };
    let response = match call.send_string(&request_body.to_string()) {
        Ok(response) => response,
        Err(ureq::Error::Status(status, response)) => {
            let text = response.into_string().unwrap_or_default();
            let detail = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v.get("error").map(error_message))
                .unwrap_or_else(|| text.chars().take(500).collect());
            let hint = match status {
                401 | 403 if options.credential.is_some() => {
                    " (update the API key in Agent > Providers)".into()
                }
                401 | 403 => format!(" (check {})", options.api_key_env),
                404 => " (check --base-url and the model name)".into(),
                429 => " (rate limited; try again later)".into(),
                _ => String::new(),
            };
            let detail = key
                .as_deref()
                .filter(|k| !k.is_empty())
                .map_or_else(|| detail.clone(), |key| detail.replace(key, "[redacted]"));
            return fail(&mut out, format!("HTTP {status}: {detail}{hint}"));
        }
        Err(e) => {
            let message = format!("cannot reach {url}: {e}");
            let message = key
                .as_deref()
                .filter(|k| !k.is_empty())
                .map_or_else(|| message.clone(), |key| message.replace(key, "[redacted]"));
            return fail(&mut out, message);
        }
    };
    let mut stream = Stream::default();
    let mut anthropic_stream = anthropic::Stream::default();
    for line in BufReader::new(response.into_reader()).lines() {
        let line = match line {
            Ok(line) => line,
            Err(e) => return fail(&mut out, format!("connection lost: {e}")),
        };
        let events = if anthropic {
            anthropic_stream.line(&line)
        } else {
            stream.line(&line)
        };
        for event in events {
            let error = matches!(event, Event::Error { .. });
            if !emit(&mut out, &event) {
                return 1;
            }
            if error {
                return 1;
            }
        }
        if stream.done || anthropic_stream.done {
            return 0;
        }
    }
    if anthropic {
        return fail(
            &mut out,
            "Anthropic connection ended before message_stop".into(),
        );
    }
    for event in stream.finish() {
        emit(&mut out, &event);
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use re_agent::{conversation::ToolCall, protocol::WireTool};
    use std::{net::TcpListener, thread};

    fn request() -> ModelRequest {
        ModelRequest {
            api: 0,
            model: "gpt-test".into(),
            system: "sys".into(),
            messages: vec![
                WireMessage::User {
                    content: "hi".into(),
                },
                WireMessage::Assistant {
                    content: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "c1".into(),
                        name: "read_buffer".into(),
                        arguments: json!({"start_line": 1}),
                    }],
                },
                WireMessage::Tool {
                    tool_call_id: "c1".into(),
                    content: "1| x".into(),
                },
            ],
            tools: vec![WireTool {
                name: "read_buffer".into(),
                description: "Read".into(),
                input_schema: json!({"type": "object"}),
            }],
            max_output_tokens: 100,
        }
    }

    #[test]
    fn builds_a_chat_completions_body() {
        let body = body(&request(), &Options::default());
        assert_eq!(body["model"], "gpt-test");
        assert_eq!(body["stream"], true);
        assert_eq!(body["max_tokens"], 100);
        assert_eq!(
            body["messages"][0],
            json!({"role": "system", "content": "sys"})
        );
        assert_eq!(
            body["messages"][2]["tool_calls"][0]["function"]["arguments"],
            "{\"start_line\":1}"
        );
        assert_eq!(body["messages"][3]["role"], "tool");
        assert_eq!(
            body["tools"][0]["function"]["parameters"],
            json!({"type": "object"})
        );
        let options = parse_args(&[
            "--model".into(),
            "llama3".into(),
            "--max-tokens-field".into(),
            "max_completion_tokens".into(),
            "--no-usage".into(),
        ])
        .unwrap();
        let body = super::body(&request(), &options);
        assert_eq!(body["model"], "llama3");
        assert_eq!(body["max_completion_tokens"], 100);
        assert!(body.get("stream_options").is_none());
        assert!(parse_args(&["--bogus".into()]).is_err());
        assert!(parse_args(&["--model".into()]).is_err());
    }

    #[test]
    fn streams_text_and_assembles_tool_calls() {
        let mut stream = Stream::default();
        let mut events = Vec::new();
        for line in [
            r#"data: {"choices":[{"delta":{"content":"Hel"}}]}"#,
            "",
            r#"data: {"choices":[{"delta":{"content":"lo"}}]}"#,
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_a","function":{"name":"edit_buffer","arguments":"{\"ed"}}]}}]}"#,
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"its\":[]}"}}]}}]}"#,
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":1,"id":"call_b","function":{"name":"read_buffer","arguments":""}}]}}]}"#,
            r#"data: {"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
            r#"data: {"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":5}}"#,
            "data: [DONE]",
        ] {
            events.extend(stream.line(line));
        }
        assert_eq!(
            events,
            vec![
                Event::Text {
                    delta: "Hel".into()
                },
                Event::Text { delta: "lo".into() },
                Event::Usage {
                    input_tokens: 10,
                    output_tokens: 5
                },
                Event::ToolCall {
                    id: "call_a".into(),
                    name: "edit_buffer".into(),
                    arguments: json!({"edits": []})
                },
                Event::ToolCall {
                    id: "call_b".into(),
                    name: "read_buffer".into(),
                    arguments: json!({})
                },
                Event::Done {
                    reason: "tool_calls".into()
                },
            ]
        );
        assert!(stream.finish().is_empty());
        let mut bad = Stream::default();
        bad.line(r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"x","function":{"name":"f","arguments":"{oops"}}]}}]}"#);
        assert!(
            matches!(&bad.finish()[0], Event::ToolCall { arguments, .. } if arguments["_invalid_json"] == "{oops")
        );
    }

    /// Serves one canned HTTP response; returns the URL and the request text.
    fn serve(status: &str, body: &'static str) -> (String, thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let status = status.to_owned();
        let handle = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(socket.try_clone().unwrap());
            let mut head = String::new();
            let mut length = 0;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = v.trim().parse().unwrap();
                }
                head.push_str(&line);
                if line == "\r\n" {
                    break;
                }
            }
            let mut body_in = vec![0; length];
            reader.read_exact(&mut body_in).unwrap();
            write!(
                socket,
                "HTTP/1.1 {status}\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
            head + &String::from_utf8(body_in).unwrap()
        });
        (url, handle)
    }

    #[test]
    fn end_to_end_against_a_local_server() {
        let (url, server) = serve(
            "200 OK",
            "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
        );
        // SAFETY: tests in this crate do not read this variable concurrently.
        unsafe { std::env::set_var("RE_TEST_KEY", "sekret") };
        let mut out = Vec::new();
        let input = serde_json::to_string(&request()).unwrap();
        let args = ["--base-url", &url, "--api-key-env", "RE_TEST_KEY"].map(String::from);
        assert_eq!(run(&args, input.as_bytes(), &mut out), 0);
        let seen = server.join().unwrap();
        assert!(seen.starts_with("POST /chat/completions"), "{seen}");
        assert!(
            seen.contains("Authorization: Bearer sekret")
                || seen.contains("authorization: Bearer sekret")
        );
        let events: Vec<Event> = String::from_utf8(out)
            .unwrap()
            .lines()
            .filter_map(re_agent::protocol::parse_event)
            .collect();
        assert_eq!(
            events,
            vec![
                Event::Text { delta: "ok".into() },
                Event::Done {
                    reason: "stop".into()
                }
            ]
        );
    }

    #[test]
    fn http_errors_become_error_events_with_hints() {
        let (url, server) = serve("401 Unauthorized", r#"{"error":{"message":"bad key"}}"#);
        let mut out = Vec::new();
        let input = serde_json::to_string(&request()).unwrap();
        let args = ["--base-url", &url, "--api-key-env", "RE_TEST_MISSING"].map(String::from);
        assert_eq!(run(&args, input.as_bytes(), &mut out), 1);
        server.join().unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("HTTP 401: bad key (check RE_TEST_MISSING)"),
            "{text}"
        );
        let mut out = Vec::new();
        assert_eq!(run(&[], "not json".as_bytes(), &mut out), 1);
        assert!(String::from_utf8(out).unwrap().contains("invalid request"));
    }

    #[test]
    fn anthropic_http_uses_messages_api_and_native_tool_schema() {
        let (url, server) = serve(
            "200 OK",
            concat!(
                "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":12}}}\n\n",
                "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"hello\"}}\n\n",
                "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":3}}\n\n",
                "data: {\"type\":\"message_stop\"}\n\n"
            ),
        );
        let input = serde_json::to_string(&request()).unwrap();
        let args = [
            "--backend",
            "anthropic",
            "--base-url",
            &url,
            "--api-key-env",
            "RE_TEST_ANTHROPIC_KEY",
        ]
        .map(String::from);
        // Unique fake key, never the user's credentials.
        unsafe { std::env::set_var("RE_TEST_ANTHROPIC_KEY", "fake-test-key") };
        let mut out = Vec::new();
        assert_eq!(run(&args, input.as_bytes(), &mut out), 0);
        let seen = server.join().unwrap();
        assert!(seen.starts_with("POST /messages"), "{seen}");
        assert!(seen.to_lowercase().contains("x-api-key: fake-test-key"));
        assert!(
            seen.to_lowercase()
                .contains("anthropic-version: 2023-06-01")
        );
        let events: Vec<Event> = String::from_utf8(out)
            .unwrap()
            .lines()
            .filter_map(re_agent::protocol::parse_event)
            .collect();
        assert_eq!(
            events,
            vec![
                Event::Text {
                    delta: "hello".into()
                },
                Event::Usage {
                    input_tokens: 12,
                    output_tokens: 3
                },
                Event::Done {
                    reason: "stop".into()
                }
            ]
        );
        let body: Value = serde_json::from_str(seen.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(body["tools"][0]["name"], "read_buffer");
        assert_eq!(body["messages"][2]["content"][0]["tool_use_id"], "c1");
    }

    #[test]
    fn api_error_diagnostics_do_not_echo_the_api_key() {
        let (url, server) = serve(
            "401 Unauthorized",
            r#"{"error":{"message":"Invalid key test-key-that-must-not-appear"}}"#,
        );
        // Unique fake credential; never reads or changes the user's key.
        unsafe { std::env::set_var("RE_TEST_REDACT_KEY", "test-key-that-must-not-appear") };
        let args = ["--base-url", &url, "--api-key-env", "RE_TEST_REDACT_KEY"].map(str::to_owned);
        let input = serde_json::to_string(&request()).unwrap();
        let mut out = Vec::new();
        assert_eq!(run(&args, input.as_bytes(), &mut out), 1);
        server.join().unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(!text.contains("test-key-that-must-not-appear"));
        assert!(text.contains("[redacted]"));
    }
}
