//! The agent loop for one conversation, as a pure state machine.
//!
//! ```text
//! Idle --user--> Streaming --done(no tools)--> Idle
//!                   |done(tool calls)
//!                   v
//!   Tools: Preparing --run--> Running --result--> next call | Streaming
//!             |ask
//!             v
//!          Approving --accept--> Running
//!                    --reject--> (result "rejected") next call | Streaming
//! any --cancel--> Idle (unfinished calls get "cancelled" results)
//! ```
//!
//! The session never runs tools or contacts a model itself; it returns
//! [`Effect`]s for the host. Every tool call the model makes gets exactly
//! one tool result in the conversation (providers require that).

use std::collections::HashSet;

use crate::{
    conversation::{Conversation, Message, ToolCall},
    protocol::{Event, ModelRequest},
    tools::{Approval, Prepared, Remember, ToolOutput, ToolSpec},
};

/// Default tool calls per user message before the agent must stop.
pub const MAX_STEPS: usize = 25;

#[derive(Debug, PartialEq)]
pub enum Effect {
    /// Start a model turn (the host fills in the model and provider).
    StartModel(ModelRequest),
    /// Prepare this tool call (validate + `Tool::prepare`, off the UI).
    Prepare(ToolCall),
    /// Run the action prepared for this call.
    Run { call_id: String },
    /// Show this approval (the host may answer at once if it already holds
    /// a persistent approval for `remember`).
    Ask { call_id: String, approval: Approval },
    /// The conversation changed: save it.
    Save,
    /// The turn is over.
    Idle,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Phase {
    Preparing,
    Approving(Approval),
    Running,
}

#[derive(Clone, Debug, PartialEq)]
pub enum State {
    Idle,
    /// Waiting for / receiving the model's reply.
    Streaming {
        text: String,
        calls: Vec<ToolCall>,
    },
    /// Working through the tool calls of the last reply.
    Tools {
        calls: Vec<ToolCall>,
        index: usize,
        phase: Phase,
    },
}

pub struct Session {
    pub conversation: Conversation,
    system: String,
    tools: Vec<ToolSpec>,
    state: State,
    steps: usize,
    max_steps: usize,
    /// Approvals granted for the rest of this conversation.
    granted: HashSet<String>,
    pub usage: (u64, u64),
}

impl Session {
    pub fn new(conversation: Conversation, system: String, tools: Vec<ToolSpec>) -> Self {
        Self {
            conversation,
            system,
            tools,
            state: State::Idle,
            steps: 0,
            max_steps: MAX_STEPS,
            granted: HashSet::new(),
            usage: (0, 0),
        }
    }

    pub fn with_max_steps(mut self, max: usize) -> Self {
        self.max_steps = max.max(1);
        self
    }

    pub fn state(&self) -> &State {
        &self.state
    }

    pub fn is_idle(&self) -> bool {
        self.state == State::Idle
    }

    /// The reply being streamed, if any.
    pub fn streaming_text(&self) -> Option<&str> {
        match &self.state {
            State::Streaming { text, .. } => Some(text),
            _ => None,
        }
    }

    /// The approval waiting for the user, if any.
    pub fn pending_approval(&self) -> Option<(&ToolCall, &Approval)> {
        match &self.state {
            State::Tools {
                calls,
                index,
                phase: Phase::Approving(approval),
            } => Some((&calls[*index], approval)),
            _ => None,
        }
    }

    /// Name of the tool call `call_id` if it is the current one.
    pub fn pending_call_name(&self, call_id: &str) -> Option<String> {
        match &self.state {
            State::Tools { calls, index, .. } if calls[*index].id == call_id => {
                Some(calls[*index].name.clone())
            }
            _ => None,
        }
    }

    pub fn tools(&self) -> &[ToolSpec] {
        &self.tools
    }

    pub fn request(&self) -> ModelRequest {
        ModelRequest::new(&self.system, &self.conversation.messages, &self.tools)
    }

    /// A user message. Ignored (returns nothing) unless idle.
    pub fn send(&mut self, text: &str) -> Vec<Effect> {
        if !self.is_idle() || text.trim().is_empty() {
            return Vec::new();
        }
        self.conversation.push(Message::User {
            text: text.to_owned(),
        });
        self.steps = 0;
        self.start_model()
    }

    fn start_model(&mut self) -> Vec<Effect> {
        self.state = State::Streaming {
            text: String::new(),
            calls: Vec::new(),
        };
        vec![Effect::Save, Effect::StartModel(self.request())]
    }

    /// An event from the provider. Stale events (after cancel) are ignored.
    pub fn on_event(&mut self, event: Event) -> Vec<Effect> {
        let State::Streaming { text, calls } = &mut self.state else {
            return Vec::new();
        };
        match event {
            Event::Text { delta } => {
                text.push_str(&delta);
                Vec::new()
            }
            Event::ToolCall {
                id,
                name,
                arguments,
            } => {
                let id = if id.is_empty() || calls.iter().any(|c| c.id == id) {
                    format!("call{}", calls.len() + 1)
                } else {
                    id
                };
                calls.push(ToolCall {
                    id,
                    name,
                    arguments,
                });
                Vec::new()
            }
            Event::Usage {
                input_tokens,
                output_tokens,
            } => {
                self.usage.0 += input_tokens;
                self.usage.1 += output_tokens;
                Vec::new()
            }
            Event::Done { reason } => {
                let text = std::mem::take(text);
                let calls = std::mem::take(calls);
                if !text.is_empty() || !calls.is_empty() {
                    self.conversation.push(Message::Assistant {
                        text,
                        tool_calls: calls.clone(),
                    });
                }
                if reason == "length" && calls.is_empty() {
                    self.conversation.push(Message::Notice {
                        text: "The reply was cut off (output limit).".into(),
                    });
                }
                if calls.is_empty() {
                    self.state = State::Idle;
                    return vec![Effect::Save, Effect::Idle];
                }
                let first = calls[0].clone();
                self.state = State::Tools {
                    calls,
                    index: 0,
                    phase: Phase::Preparing,
                };
                vec![Effect::Save, Effect::Prepare(first)]
            }
            Event::Error { message } => {
                let partial = std::mem::take(text);
                if !partial.is_empty() {
                    self.conversation.push(Message::Assistant {
                        text: partial,
                        tool_calls: Vec::new(),
                    });
                }
                self.conversation.push(Message::Notice {
                    text: format!("Error: {message}"),
                });
                self.state = State::Idle;
                vec![Effect::Save, Effect::Idle]
            }
        }
    }

    fn current(&self, call_id: &str, phase: &Phase) -> bool {
        matches!(&self.state, State::Tools { calls, index, phase: p }
            if calls[*index].id == call_id && std::mem::discriminant(p) == std::mem::discriminant(phase))
    }

    /// The result of preparing the current call. Pass the `Approval` (or the
    /// error) only; the host keeps the action.
    pub fn on_prepared(
        &mut self,
        call_id: &str,
        prepared: Result<Option<Approval>, String>,
    ) -> Vec<Effect> {
        if !self.current(call_id, &Phase::Preparing) {
            return Vec::new();
        }
        match prepared {
            Err(error) => self.on_tool_result(call_id, ToolOutput::error(error), true),
            Ok(None) => {
                self.set_phase(Phase::Running);
                vec![Effect::Run {
                    call_id: call_id.to_owned(),
                }]
            }
            Ok(Some(approval)) => {
                if let Remember::Conversation(key) = &approval.remember
                    && self.granted.contains(key)
                {
                    self.set_phase(Phase::Running);
                    return vec![Effect::Run {
                        call_id: call_id.to_owned(),
                    }];
                }
                self.set_phase(Phase::Approving(approval.clone()));
                vec![Effect::Ask {
                    call_id: call_id.to_owned(),
                    approval,
                }]
            }
        }
    }

    /// The user's answer to the current approval.
    pub fn on_approval(&mut self, call_id: &str, accepted: bool) -> Vec<Effect> {
        let Some((call, approval)) = self.pending_approval() else {
            return Vec::new();
        };
        if call.id != call_id {
            return Vec::new();
        }
        if !accepted {
            return self.on_tool_result(
                call_id,
                ToolOutput::error("The user rejected this action."),
                true,
            );
        }
        if let Remember::Conversation(key) = &approval.remember {
            self.granted.insert(key.clone());
        }
        self.set_phase(Phase::Running);
        vec![Effect::Run {
            call_id: call_id.to_owned(),
        }]
    }

    /// A finished tool call (or a failed preparation / rejection when
    /// `force` is set).
    pub fn on_tool_result(
        &mut self,
        call_id: &str,
        output: ToolOutput,
        force: bool,
    ) -> Vec<Effect> {
        let State::Tools {
            calls,
            index,
            phase,
        } = &self.state
        else {
            return Vec::new();
        };
        if calls[*index].id != call_id || (!force && *phase != Phase::Running) {
            return Vec::new();
        }
        let call = calls[*index].clone();
        let next = *index + 1;
        let calls = calls.clone();
        self.conversation.push(Message::Tool {
            call_id: call.id,
            name: call.name,
            content: output.content,
            is_error: output.is_error,
        });
        self.steps += 1;
        if next < calls.len() {
            let call = calls[next].clone();
            self.state = State::Tools {
                calls,
                index: next,
                phase: Phase::Preparing,
            };
            return vec![Effect::Save, Effect::Prepare(call)];
        }
        if self.steps >= self.max_steps {
            self.conversation.push(Message::Notice {
                text: format!(
                    "Stopped after {} tool calls. Send a message to continue.",
                    self.steps
                ),
            });
            self.state = State::Idle;
            return vec![Effect::Save, Effect::Idle];
        }
        self.start_model()
    }

    /// Stops the turn. Unfinished tool calls get a "cancelled" result.
    pub fn cancel(&mut self) -> Vec<Effect> {
        match std::mem::replace(&mut self.state, State::Idle) {
            State::Idle => return Vec::new(),
            State::Streaming { text, .. } => {
                if !text.is_empty() {
                    self.conversation.push(Message::Assistant {
                        text,
                        tool_calls: Vec::new(),
                    });
                }
            }
            State::Tools { calls, index, .. } => {
                for call in &calls[index..] {
                    self.conversation.push(Message::Tool {
                        call_id: call.id.clone(),
                        name: call.name.clone(),
                        content: "Cancelled by the user.".into(),
                        is_error: true,
                    });
                }
            }
        }
        self.conversation.push(Message::Notice {
            text: "Stopped.".into(),
        });
        vec![Effect::Save, Effect::Idle]
    }

    fn set_phase(&mut self, new: Phase) {
        if let State::Tools { phase, .. } = &mut self.state {
            *phase = new;
        }
    }
}

/// Splits a `Prepared` into what the session needs and the action the host
/// keeps.
pub fn split(prepared: Prepared) -> (Option<Approval>, crate::tools::Action) {
    match prepared {
        Prepared::Run(action) => (None, action),
        Prepared::Ask(approval, action) => (Some(approval), action),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;

    use super::*;
    use crate::{
        conversation::Subject,
        tools::{Action, Risk, ToolContext, ToolRegistry, run_worker, tests::Echo},
    };

    fn registry() -> ToolRegistry {
        let mut r = ToolRegistry::default();
        r.register(Arc::new(Echo {
            name: "echo",
            ask: None,
        }));
        r.register(Arc::new(Echo {
            name: "write",
            ask: Some(Remember::Once),
        }));
        r.register(Arc::new(Echo {
            name: "outside",
            ask: Some(Remember::Conversation("folder:/x".into())),
        }));
        r
    }

    /// Drives a session like a host would: prepares and runs tools
    /// synchronously, answers approvals with `answers`, returns the model
    /// requests it was asked to make.
    fn drive(
        session: &mut Session,
        registry: &ToolRegistry,
        mut effects: Vec<Effect>,
        turns: &mut std::collections::VecDeque<Vec<Event>>,
        answers: &mut std::collections::VecDeque<bool>,
    ) -> Vec<ModelRequest> {
        let mut requests = Vec::new();
        let mut actions = std::collections::HashMap::new();
        let ctx = ToolContext::default();
        while let Some(effect) = (!effects.is_empty()).then(|| effects.remove(0)) {
            match effect {
                Effect::StartModel(request) => {
                    requests.push(request);
                    for event in turns.pop_front().expect("scripted turn") {
                        effects.extend(session.on_event(event));
                    }
                }
                Effect::Prepare(call) => {
                    let result = registry.prepare(&ctx, &call.name, &call.arguments);
                    let for_session = match result {
                        Ok(prepared) => {
                            let (approval, action) = split(prepared);
                            actions.insert(call.id.clone(), action);
                            Ok(approval)
                        }
                        Err(e) => Err(e),
                    };
                    effects.extend(session.on_prepared(&call.id, for_session));
                }
                Effect::Ask { call_id, .. } => {
                    let answer = answers.pop_front().expect("scripted answer");
                    effects.extend(session.on_approval(&call_id, answer));
                }
                Effect::Run { call_id } => {
                    let Some(Action::Worker(run)) = actions.remove(&call_id) else {
                        panic!("no action");
                    };
                    let output = run_worker(run, &ctx.cancel);
                    effects.extend(session.on_tool_result(&call_id, output, false));
                }
                Effect::Save | Effect::Idle => {}
            }
        }
        requests
    }

    fn call(id: &str, name: &str, text: &str) -> Event {
        Event::ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: json!({ "text": text }),
        }
    }

    fn done() -> Event {
        Event::Done {
            reason: "stop".into(),
        }
    }

    fn session() -> Session {
        let registry = registry();
        Session::new(
            Conversation::new("file-agent", Subject::Global),
            "system".into(),
            registry.specs(&["echo".into(), "write".into(), "outside".into()]),
        )
    }

    #[test]
    fn text_only_turns_and_streaming() {
        let mut s = session();
        let effects = s.send("hello");
        assert!(
            matches!(&effects[1], Effect::StartModel(r) if r.system == "system" && r.tools.len() == 3)
        );
        s.on_event(Event::Text {
            delta: "Hi ".into(),
        });
        s.on_event(Event::Text {
            delta: "there".into(),
        });
        assert_eq!(s.streaming_text(), Some("Hi there"));
        assert!(s.send("ignored while busy").is_empty());
        assert_eq!(s.on_event(done()), vec![Effect::Save, Effect::Idle]);
        assert_eq!(
            s.conversation.messages.last(),
            Some(&Message::Assistant {
                text: "Hi there".into(),
                tool_calls: vec![]
            })
        );
        assert_eq!(s.conversation.title, "hello");
    }

    #[test]
    fn tool_calls_run_and_results_go_back_to_the_model() {
        let registry = registry();
        let mut s = session();
        let mut turns = vec![
            vec![call("c1", "echo", "one"), call("c2", "echo", "two"), done()],
            vec![
                Event::Text {
                    delta: "Done.".into(),
                },
                done(),
            ],
        ]
        .into();
        let effects = s.send("go");
        let requests = drive(
            &mut s,
            &registry,
            effects,
            &mut turns,
            &mut Default::default(),
        );
        assert_eq!(requests.len(), 2);
        let second = serde_json::to_value(&requests[1]).unwrap();
        let messages = second["messages"].as_array().unwrap();
        assert_eq!(messages[1]["tool_calls"].as_array().unwrap().len(), 2);
        assert_eq!(
            messages[2],
            json!({"role": "tool", "tool_call_id": "c1", "content": "one"})
        );
        assert_eq!(
            messages[3],
            json!({"role": "tool", "tool_call_id": "c2", "content": "two"})
        );
        assert!(s.is_idle());
    }

    #[test]
    fn approvals_accept_reject_and_remember_per_conversation() {
        let registry = registry();
        let mut s = session();
        let mut turns = vec![
            vec![call("c1", "write", "w1"), call("c2", "write", "w2"), done()],
            vec![done()],
        ]
        .into();
        let mut answers = vec![true, false].into();
        let effects = s.send("edit");
        drive(&mut s, &registry, effects, &mut turns, &mut answers);
        let results: Vec<_> = s
            .conversation
            .messages
            .iter()
            .filter_map(|m| match m {
                Message::Tool {
                    content, is_error, ..
                } => Some((content.clone(), *is_error)),
                _ => None,
            })
            .collect();
        assert_eq!(
            results,
            vec![
                ("w1".into(), false),
                ("The user rejected this action.".into(), true)
            ]
        );
        // "Conversation" approvals are asked once.
        let mut turns = vec![
            vec![call("a", "outside", "1"), call("b", "outside", "2"), done()],
            vec![done()],
        ]
        .into();
        let mut answers = vec![true].into(); // only one question
        let effects = s.send("read outside");
        drive(&mut s, &registry, effects, &mut turns, &mut answers);
        assert!(answers.is_empty());
        assert!(s.is_idle());
    }

    #[test]
    fn bad_calls_become_errors_the_model_can_fix() {
        let registry = registry();
        let mut s = session();
        let mut turns = vec![
            vec![
                Event::ToolCall {
                    id: "c1".into(),
                    name: "nope".into(),
                    arguments: json!({}),
                },
                Event::ToolCall {
                    id: "c2".into(),
                    name: "echo".into(),
                    arguments: json!({"x": 1}),
                },
                done(),
            ],
            vec![done()],
        ]
        .into();
        let effects = s.send("x");
        let requests = drive(
            &mut s,
            &registry,
            effects,
            &mut turns,
            &mut Default::default(),
        );
        let last = serde_json::to_string(&requests[1]).unwrap();
        assert!(last.contains("ERROR: unknown tool `nope`"));
        assert!(last.contains("missing required field `text`"));
    }

    #[test]
    fn cancel_closes_every_open_call_and_step_limit_stops_loops() {
        let mut s = session();
        s.send("x");
        s.on_event(call("c1", "write", "a"));
        s.on_event(call("c2", "echo", "b"));
        s.on_event(done());
        s.on_prepared(
            "c1",
            Ok(Some(Approval {
                title: "t".into(),
                detail: String::new(),
                diff: vec![],
                risk: Risk::Write,
                remember: Remember::Once,
            })),
        );
        assert!(s.pending_approval().is_some());
        assert_eq!(s.cancel(), vec![Effect::Save, Effect::Idle]);
        let tool_results = s
            .conversation
            .messages
            .iter()
            .filter(|m| matches!(m, Message::Tool { .. }))
            .count();
        assert_eq!(tool_results, 2);
        // Late events and answers are ignored.
        assert!(
            s.on_event(Event::Text {
                delta: "late".into()
            })
            .is_empty()
        );
        assert!(s.on_approval("c1", true).is_empty());

        let registry = registry();
        let mut s = session().with_max_steps(3);
        let mut turns: std::collections::VecDeque<_> = (0..10)
            .map(|i| vec![call(&format!("c{i}"), "echo", "again"), done()])
            .collect();
        let effects = s.send("loop forever");
        let requests = drive(
            &mut s,
            &registry,
            effects,
            &mut turns,
            &mut Default::default(),
        );
        assert_eq!(requests.len(), 3);
        assert!(
            matches!(s.conversation.messages.last(), Some(Message::Notice { text }) if text.contains("Stopped after 3"))
        );
    }

    #[test]
    fn provider_errors_keep_partial_text_and_end_the_turn() {
        let mut s = session();
        s.send("x");
        s.on_event(Event::Text {
            delta: "part".into(),
        });
        assert_eq!(
            s.on_event(Event::Error {
                message: "rate limited".into()
            }),
            vec![Effect::Save, Effect::Idle]
        );
        assert!(
            matches!(&s.conversation.messages[1], Message::Assistant { text, .. } if text == "part")
        );
        assert!(
            matches!(&s.conversation.messages[2], Message::Notice { text } if text.contains("rate limited"))
        );
    }
}
