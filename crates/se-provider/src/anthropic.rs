//! Anthropic's Messages API wire format: the request body and the SSE
//! stream, translated to and from SE's provider-neutral types.
//!
//! Pure functions only. The HTTP request, its credential handling and
//! every safety check are the shared client's (`openai_compatible.rs`);
//! this module only knows how Anthropic shapes its JSON.

use crate::{ChatRequest, Role, StreamEvent};

/// The API version SE is written against. Sent on every request, as
/// Anthropic requires.
pub const API_VERSION: &str = "2023-06-01";

/// Builds the `/v1/messages` body.
///
/// The differences from the OpenAI shape that matter:
/// - system prompts go in a top-level `system`, not a message;
/// - an assistant's tool calls are `tool_use` content blocks whose
///   `input` is a JSON object, not a string of JSON;
/// - tool results are `tool_result` blocks inside a *user* message, and
///   consecutive results must share one message (roles must alternate).
pub fn request_body(request: &ChatRequest) -> serde_json::Value {
    let system: Vec<&str> = request
        .messages
        .iter()
        .filter(|m| m.role == Role::System)
        .map(|m| m.content.as_str())
        .collect();

    let mut messages: Vec<serde_json::Value> = Vec::new();
    for m in request.messages.iter().filter(|m| m.role != Role::System) {
        let (role, blocks) = match m.role {
            Role::Tool => (
                "user",
                vec![serde_json::json!({
                    "type": "tool_result",
                    "tool_use_id": m.tool_call_id.clone().unwrap_or_default(),
                    "content": m.content,
                })],
            ),
            Role::Assistant => {
                let mut blocks = Vec::new();
                if !m.content.is_empty() {
                    blocks.push(serde_json::json!({"type": "text", "text": m.content}));
                }
                for c in &m.tool_calls {
                    // A model's arguments that are not valid JSON are sent
                    // as an empty object rather than failing the request;
                    // the tool result already told the model what failed.
                    let input: serde_json::Value = serde_json::from_str(&c.arguments)
                        .ok()
                        .filter(serde_json::Value::is_object)
                        .unwrap_or_else(|| serde_json::json!({}));
                    blocks.push(serde_json::json!({
                        "type": "tool_use",
                        "id": c.id,
                        "name": c.name,
                        "input": input,
                    }));
                }
                ("assistant", blocks)
            }
            _ => (
                "user",
                vec![serde_json::json!({"type": "text", "text": m.content})],
            ),
        };
        if blocks.is_empty() {
            continue;
        }
        // Merge into the previous message when the role repeats.
        match messages.last_mut() {
            Some(last) if last["role"] == role => {
                last["content"]
                    .as_array_mut()
                    .expect("content is always an array here")
                    .extend(blocks);
            }
            _ => messages.push(serde_json::json!({"role": role, "content": blocks})),
        }
    }

    // Prompt caching: one breakpoint on the system prompt (a specialist's
    // body, which repeats every turn) and one on the newest block, so the
    // next turn re-reads the whole prefix at the cached rate. Prompts under
    // the model's minimum cacheable length are simply not cached.
    if let Some(block) = messages
        .last_mut()
        .and_then(|m| m["content"].as_array_mut())
        .and_then(|blocks| blocks.last_mut())
    {
        block["cache_control"] = serde_json::json!({"type": "ephemeral"});
    }
    let mut body = serde_json::json!({
        "model": request.model,
        "max_tokens": request.max_tokens,
        "messages": messages,
        "stream": true,
    });
    if !system.is_empty() {
        body["system"] = serde_json::json!([{
            "type": "text",
            "text": system.join("\n\n"),
            "cache_control": {"type": "ephemeral"},
        }]);
    }
    if !request.tools.is_empty() {
        body["tools"] = request
            .tools
            .iter()
            .map(|t| {
                serde_json::json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": t.parameters,
                })
            })
            .collect();
    }
    body
}

/// State that must outlive single frames: tool calls are streamed as a
/// start block plus JSON fragments, and input usage arrives first while
/// output usage arrives last.
#[derive(Debug, Default)]
pub struct StreamState {
    /// `(block index, id, name, partial JSON)` for open `tool_use` blocks.
    tools: Vec<(u64, String, String, String)>,
    input_tokens: Option<u64>,
    cached_input_tokens: Option<u64>,
}

/// Translates one SSE `data:` payload into SE's events.
///
/// Stop reasons are mapped onto the OpenAI vocabulary the shared client
/// and the runner already act on: `tool_use` -> `tool_calls`,
/// `max_tokens` -> `length` (truncated), `refusal` -> `content_filter`,
/// anything else -> `stop`. `message_stop` is the end of the stream.
pub fn parse_frame(payload: &str, state: &mut StreamState) -> Vec<StreamEvent> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(payload.trim()) else {
        return Vec::new();
    };
    let str_at =
        |v: &serde_json::Value, k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_string);
    let mut events = Vec::new();
    match value.get("type").and_then(|t| t.as_str()).unwrap_or("") {
        "message_start" => {
            let usage = &value["message"]["usage"];
            // Anthropic counts cache reads and writes apart from `input_tokens`; SE's
            // `Usage` treats cached input as part of input, as OpenAI and Codex report it.
            state.input_tokens = [
                "input_tokens",
                "cache_creation_input_tokens",
                "cache_read_input_tokens",
            ]
            .iter()
            .filter_map(|k| usage.get(*k).and_then(|v| v.as_u64()))
            .reduce(|a, b| a + b);
            state.cached_input_tokens = usage
                .get("cache_read_input_tokens")
                .and_then(|v| v.as_u64());
        }
        "content_block_start" => {
            let block = &value["content_block"];
            if block["type"] == "tool_use" {
                let index = value["index"].as_u64().unwrap_or(0);
                state.tools.push((
                    index,
                    str_at(block, "id").unwrap_or_default(),
                    str_at(block, "name").unwrap_or_default(),
                    String::new(),
                ));
            }
        }
        "content_block_delta" => {
            let delta = &value["delta"];
            match delta["type"].as_str().unwrap_or("") {
                "text_delta" => {
                    if let Some(text) = str_at(delta, "text").filter(|t| !t.is_empty()) {
                        events.push(StreamEvent::TextDelta(text));
                    }
                }
                "thinking_delta" => {
                    if let Some(text) = str_at(delta, "thinking").filter(|t| !t.is_empty()) {
                        events.push(StreamEvent::ReasoningDelta(text));
                    }
                }
                "input_json_delta" => {
                    let index = value["index"].as_u64().unwrap_or(0);
                    if let Some(slot) = state.tools.iter_mut().find(|(i, ..)| *i == index) {
                        slot.3
                            .push_str(delta["partial_json"].as_str().unwrap_or(""));
                    }
                }
                _ => {}
            }
        }
        "message_delta" => {
            events.push(StreamEvent::Usage {
                input_tokens: state.input_tokens,
                output_tokens: value["usage"].get("output_tokens").and_then(|v| v.as_u64()),
                cached_input_tokens: state.cached_input_tokens,
            });
            if let Some(reason) = value["delta"].get("stop_reason").and_then(|r| r.as_str()) {
                let mapped = match reason {
                    "tool_use" => "tool_calls",
                    "max_tokens" => "length",
                    "refusal" => "content_filter",
                    _ => "stop",
                };
                if mapped == "tool_calls" {
                    for (_, id, name, json) in state.tools.drain(..) {
                        // A tool with no parameters streams no fragments.
                        let arguments = if json.trim().is_empty() {
                            "{}".to_string()
                        } else {
                            json
                        };
                        events.push(StreamEvent::ToolCallRequested {
                            id,
                            name,
                            arguments,
                        });
                    }
                }
                events.push(StreamEvent::FinishReason(mapped.to_string()));
            }
        }
        "message_stop" => events.push(StreamEvent::Done),
        "error" => {
            let detail = value["error"]
                .get("message")
                .and_then(|m| m.as_str())
                .map(str::to_string)
                .unwrap_or_else(|| value["error"].to_string());
            events.push(StreamEvent::Error(detail));
        }
        _ => {} // ping, content_block_stop: nothing to report
    }
    events
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Message, ToolCallRecord, ToolDefinition};

    fn req(messages: Vec<Message>) -> ChatRequest {
        ChatRequest {
            model: "claude-sonnet".into(),
            messages,
            max_tokens: 1024,
            tools: vec![ToolDefinition {
                name: "read".into(),
                description: "Reads a file.".into(),
                parameters: serde_json::json!({"type": "object"}),
            }],
        }
    }

    #[test]
    fn the_body_uses_anthropics_shape_for_system_tools_and_results() {
        let body = request_body(&req(vec![
            Message::text(Role::System, "be careful"),
            Message::text(Role::User, "read two files"),
            Message::assistant_tool_calls(vec![
                ToolCallRecord {
                    id: "toolu_1".into(),
                    name: "read".into(),
                    arguments: r#"{"path":"a"}"#.into(),
                },
                ToolCallRecord {
                    id: "toolu_2".into(),
                    name: "read".into(),
                    arguments: "not json".into(),
                },
            ]),
            Message::tool_result("toolu_1", "A"),
            Message::tool_result("toolu_2", "B"),
        ]));
        assert_eq!(body["system"][0]["text"], "be careful");
        assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(body["stream"], true);
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3, "user, assistant, one merged user: {msgs:?}");
        assert_eq!(
            msgs[1]["content"][0]["input"]["path"], "a",
            "input is an object"
        );
        assert_eq!(msgs[1]["content"][1]["input"], serde_json::json!({}));
        let results = msgs[2]["content"].as_array().unwrap();
        assert_eq!(msgs[2]["role"], "user");
        assert_eq!(results.len(), 2, "consecutive results share one message");
        assert_eq!(results[1]["tool_use_id"], "toolu_2");
        assert_eq!(
            results[1]["cache_control"]["type"], "ephemeral",
            "newest block cached"
        );
        assert!(
            results[0].get("cache_control").is_none(),
            "only one message breakpoint"
        );
        assert_eq!(body["tools"][0]["input_schema"]["type"], "object");
    }

    fn feed(frames: &[&str]) -> Vec<StreamEvent> {
        let mut state = StreamState::default();
        frames
            .iter()
            .flat_map(|f| parse_frame(f, &mut state))
            .collect()
    }

    #[test]
    fn a_streamed_tool_call_is_reassembled_and_mapped() {
        let events = feed(&[
            r#"{"type":"message_start","message":{"usage":{"input_tokens":12,"cache_read_input_tokens":4}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Let me look."}}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_9","name":"read","input":{}}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"pa"}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"th\":\"x\"}"}}"#,
            r#"{"type":"ping"}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":30}}"#,
            r#"{"type":"message_stop"}"#,
        ]);
        assert_eq!(
            events,
            vec![
                StreamEvent::TextDelta("Let me look.".into()),
                StreamEvent::Usage {
                    input_tokens: Some(16),
                    output_tokens: Some(30),
                    cached_input_tokens: Some(4),
                },
                StreamEvent::ToolCallRequested {
                    id: "toolu_9".into(),
                    name: "read".into(),
                    arguments: r#"{"path":"x"}"#.into(),
                },
                StreamEvent::FinishReason("tool_calls".into()),
                StreamEvent::Done,
            ]
        );
    }

    #[test]
    fn stop_reasons_map_onto_what_the_runner_acts_on() {
        for (theirs, ours) in [
            ("end_turn", "stop"),
            ("max_tokens", "length"),
            ("refusal", "content_filter"),
            ("stop_sequence", "stop"),
        ] {
            let frame =
                format!(r#"{{"type":"message_delta","delta":{{"stop_reason":"{theirs}"}}}}"#);
            let events = feed(&[&frame]);
            assert_eq!(
                events.last(),
                Some(&StreamEvent::FinishReason(ours.into())),
                "{theirs}"
            );
        }
    }

    #[test]
    fn errors_and_thinking_come_through() {
        let events = feed(&[
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hmm"}}"#,
            r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
        ]);
        assert_eq!(
            events,
            vec![
                StreamEvent::ReasoningDelta("hmm".into()),
                StreamEvent::Error("Overloaded".into())
            ]
        );
    }
}
