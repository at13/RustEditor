//! Request/response messages (JSON, UTF-8), shared by every plugin kind.
//!
//! Request:
//! ```json
//! {"api": 0, "plugin": "sort-lines", "command": "sort",
//!  "inputs": ["b\na", "..."], "path": "/only/with/document/capability.txt"}
//! ```
//! Response (one of):
//! ```json
//! {"outputs": ["a\nb", "..."], "status": "optional message"}
//! {"show": "text for a read-only viewer"}
//! {"error": "what went wrong"}
//! ```

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::{
    API_VERSION, MAX_MESSAGE_BYTES,
    manifest::{Capability, Command, Output, Plugin},
};

/// What the host sends.
#[derive(Clone, Debug, Default)]
pub struct Request {
    /// Selection texts, or the whole document as one input.
    pub inputs: Vec<String>,
    /// The document's path (sent only with the `document` capability).
    pub path: Option<String>,
}

impl Request {
    pub fn to_json(&self, plugin: &Plugin, command: &Command) -> String {
        let mut value = json!({
            "api": API_VERSION,
            "plugin": plugin.manifest.id,
            "command": command.id,
            "inputs": self.inputs,
        });
        if plugin.has(Capability::Document)
            && let Some(path) = &self.path
        {
            value["path"] = json!(path);
        }
        value.to_string()
    }

    pub fn size(&self) -> usize {
        self.inputs.iter().map(String::len).sum()
    }
}

/// What the host does with a successful response.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Replace {
        outputs: Vec<String>,
        status: Option<String>,
    },
    Show {
        text: String,
        status: Option<String>,
    },
}

/// Validates a response against what the command promised.
pub fn interpret(bytes: &[u8], command: &Command, inputs: usize) -> Result<Outcome> {
    if bytes.len() > MAX_MESSAGE_BYTES {
        bail!("response is larger than {} MiB", MAX_MESSAGE_BYTES >> 20);
    }
    let text = std::str::from_utf8(bytes).context("response is not UTF-8")?;
    let value: Value = serde_json::from_str(text.trim()).context("response is not valid JSON")?;
    if let Some(error) = value.get("error") {
        bail!("{}", error.as_str().unwrap_or("failed"));
    }
    let status = value
        .get("status")
        .and_then(Value::as_str)
        .map(|s| s.chars().take(200).collect());
    match command.output {
        Output::Replace => {
            let outputs: Vec<String> = value
                .get("outputs")
                .and_then(Value::as_array)
                .context("response has no `outputs` array")?
                .iter()
                .map(|v| v.as_str().map(str::to_owned))
                .collect::<Option<_>>()
                .context("`outputs` must be strings")?;
            if outputs.len() != inputs {
                bail!("returned {} outputs for {inputs} inputs", outputs.len());
            }
            Ok(Outcome::Replace { outputs, status })
        }
        Output::Show => Ok(Outcome::Show {
            text: string_field(&value, "show")?,
            status,
        }),
    }
}

fn string_field(value: &Value, key: &str) -> Result<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .with_context(|| format!("response has no `{key}` string"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{Input, Manifest};

    fn command(output: Output) -> Command {
        Command {
            id: "c".into(),
            title: "C".into(),
            input: Input::Selection,
            output,
        }
    }

    #[test]
    fn interprets_replace_show_and_error() {
        let replace = interpret(
            br#"{"outputs":["A","B"],"status":"done"}"#,
            &command(Output::Replace),
            2,
        );
        assert_eq!(
            replace.unwrap(),
            Outcome::Replace {
                outputs: vec!["A".into(), "B".into()],
                status: Some("done".into())
            }
        );
        let show = interpret(br#"{"show":"hello"}"#, &command(Output::Show), 1).unwrap();
        assert_eq!(
            show,
            Outcome::Show {
                text: "hello".into(),
                status: None
            }
        );
        let error = interpret(br#"{"error":"nope"}"#, &command(Output::Show), 1).unwrap_err();
        assert_eq!(error.to_string(), "nope");
    }

    #[test]
    fn rejects_mismatched_or_malformed_responses() {
        let c = command(Output::Replace);
        assert!(
            interpret(br#"{"outputs":["A"]}"#, &c, 2)
                .unwrap_err()
                .to_string()
                .contains("1 outputs for 2")
        );
        assert!(interpret(b"not json", &c, 1).is_err());
        assert!(interpret(&[0xff, 0xfe], &c, 1).is_err());
        assert!(interpret(br#"{"outputs":[1]}"#, &c, 1).is_err());
        assert!(interpret(br#"{}"#, &command(Output::Show), 1).is_err());
    }

    #[test]
    fn path_is_sent_only_with_the_document_capability() {
        let manifest = |caps: &str| {
            Manifest::parse(&format!(
                r#"{{"api":0,"id":"p","name":"P","version":"1","kind":"wasm","module":"m",
                    "capabilities":[{caps}],"commands":[{{"id":"c","title":"C"}}]}}"#
            ))
            .unwrap()
        };
        let request = Request {
            inputs: vec!["x".into()],
            path: Some("/secret/file.txt".into()),
        };
        let without = Plugin {
            manifest: manifest(""),
            dir: Default::default(),
        };
        let json = request.to_json(&without, &without.manifest.commands[0]);
        assert!(!json.contains("secret"), "{json}");
        let with = Plugin {
            manifest: manifest(r#""document""#),
            dir: Default::default(),
        };
        let json = request.to_json(&with, &with.manifest.commands[0]);
        assert!(json.contains("secret"), "{json}");
        let value: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["api"], 0);
        assert_eq!(value["command"], "c");
        assert_eq!(value["inputs"][0], "x");
    }
}
