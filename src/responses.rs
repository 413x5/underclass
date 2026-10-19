//! Translation between the OpenAI Responses API and Chat Completions.
//!
//! Backends that only speak Chat Completions (Cline) declare `Backend::responses_via_chat`; the
//! proxy converts the request body before the upstream call and converts the upstream body back
//! before it reaches the client.

use serde_json::{json, Map, Value};
use std::io::Write;

/// @cc [owner:ghuntley,label:proxy] responses-request-translation
/// A Responses request MUST become a Chat Completions body: `instructions` MUST become a leading
/// `system` message, `input` items MUST become `messages` (function calls and their outputs
/// preserved as `tool_calls` and `role: tool`), and `max_output_tokens` MUST be sent as
/// `max_tokens`. Absent or `null` fields MUST be omitted rather than emitted as `null`.
/// `tool_choice` MUST be forwarded only when it is non-`null` and at least one `function` tool
/// converted, because upstreams reject `tool_choice` without `tools`.
pub fn request_to_chat(body: &Value) -> Value {
    let mut chat = Map::new();
    if let Some(model) = body.get("model") {
        chat.insert("model".into(), model.clone());
    }

    let mut messages = Vec::new();
    if let Some(instructions) = body
        .get("instructions")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
    {
        messages.push(json!({"role": "system", "content": instructions}));
    }
    match body.get("input") {
        Some(Value::String(text)) => messages.push(json!({"role": "user", "content": text})),
        Some(Value::Array(items)) => {
            for item in items {
                push_input_item(&mut messages, item);
            }
        }
        _ => {}
    }
    if !messages.is_empty() {
        chat.insert("messages".into(), Value::Array(messages));
    }

    for key in [
        "stream",
        "temperature",
        "top_p",
        "frequency_penalty",
        "presence_penalty",
        "seed",
        "stop",
        "parallel_tool_calls",
        "prompt_cache_key",
    ] {
        if let Some(value) = body.get(key).filter(|value| !value.is_null()) {
            chat.insert(key.into(), value.clone());
        }
    }
    if let Some(value) = body.get("max_output_tokens").filter(|value| !value.is_null()) {
        chat.insert("max_tokens".into(), value.clone());
    }
    if let Some(effort) = body
        .get("reasoning")
        .and_then(|reasoning| reasoning.get("effort"))
        .filter(|effort| !effort.is_null())
    {
        chat.insert("reasoning_effort".into(), effort.clone());
    }
    if let Some(format) = body.get("text").and_then(|text| text.get("format")) {
        match format.get("type").and_then(Value::as_str) {
            Some("json_schema") => {
                let mut schema = Map::new();
                schema.insert(
                    "name".into(),
                    format
                        .get("name")
                        .filter(|name| !name.is_null())
                        .cloned()
                        .unwrap_or_else(|| json!("response")),
                );
                schema.insert(
                    "schema".into(),
                    format
                        .get("schema")
                        .filter(|schema| !schema.is_null())
                        .cloned()
                        .unwrap_or_else(|| json!({})),
                );
                if let Some(strict) = format.get("strict").filter(|strict| !strict.is_null()) {
                    schema.insert("strict".into(), strict.clone());
                }
                chat.insert(
                    "response_format".into(),
                    json!({"type": "json_schema", "json_schema": Value::Object(schema)}),
                );
            }
            Some("json_object") => {
                chat.insert("response_format".into(), json!({"type": "json_object"}));
            }
            _ => {}
        }
    }
    let converted: Vec<Value> = body
        .get("tools")
        .and_then(Value::as_array)
        .map(|tools| tools.iter().filter_map(chat_tool).collect())
        .unwrap_or_default();
    if !converted.is_empty() {
        chat.insert("tools".into(), Value::Array(converted));
        if let Some(choice) = body.get("tool_choice").filter(|value| !value.is_null()) {
            chat.insert("tool_choice".into(), chat_tool_choice(choice));
        }
    }

    Value::Object(chat)
}

fn chat_tool(tool: &Value) -> Option<Value> {
    if tool.get("type").and_then(Value::as_str) != Some("function") {
        return None;
    }
    if tool.get("function").is_some() {
        return Some(tool.clone());
    }
    let name = tool.get("name").and_then(Value::as_str)?;
    let mut function = Map::new();
    function.insert("name".into(), Value::String(name.to_string()));
    if let Some(description) = tool.get("description").filter(|value| !value.is_null()) {
        function.insert("description".into(), description.clone());
    }
    // `strict` is part of the caller's schema contract: dropping it would let the routed model emit
    // arguments the client asked to be structurally impossible.
    if let Some(strict) = tool.get("strict").filter(|value| value.is_boolean()) {
        function.insert("strict".into(), strict.clone());
    }
    // A no-argument tool commonly sends `parameters: null`, which Chat upstreams reject; fall back
    // to the empty object schema exactly as for an absent key.
    function.insert(
        "parameters".into(),
        tool.get("parameters")
            .filter(|value| !value.is_null())
            .cloned()
            .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
    );
    Some(json!({"type": "function", "function": Value::Object(function)}))
}

fn chat_tool_choice(choice: &Value) -> Value {
    match choice {
        Value::String(_) => choice.clone(),
        Value::Object(map) if map.get("type").and_then(Value::as_str) == Some("function") => {
            if map.get("function").is_some() {
                choice.clone()
            } else if let Some(name) = map.get("name") {
                json!({"type": "function", "function": {"name": name}})
            } else {
                json!("auto")
            }
        }
        _ => json!("auto"),
    }
}

fn push_input_item(messages: &mut Vec<Value>, item: &Value) {
    match item.get("type").and_then(Value::as_str) {
        Some("function_call") => {
            let call_id = item.get("call_id").or_else(|| item.get("id")).cloned().unwrap_or(Value::Null);
            let name = item.get("name").cloned().unwrap_or(Value::Null);
            let arguments = match item.get("arguments") {
                Some(Value::String(arguments)) => Value::String(arguments.clone()),
                Some(other) => Value::String(other.to_string()),
                None => Value::String("{}".into()),
            };
            messages.push(json!({
                "role": "assistant",
                "content": Value::Null,
                "tool_calls": [{
                    "id": call_id,
                    "type": "function",
                    "function": {"name": name, "arguments": arguments}
                }]
            }));
        }
        Some("function_call_output") => {
            let call_id = item.get("call_id").or_else(|| item.get("id")).cloned().unwrap_or(Value::Null);
            let output = match item.get("output") {
                Some(Value::String(output)) => Value::String(output.clone()),
                Some(other) => Value::String(other.to_string()),
                None => Value::String(String::new()),
            };
            messages.push(json!({
                "role": "tool",
                "tool_call_id": call_id,
                "content": output
            }));
        }
        Some("reasoning") => {}
        _ => {
            let Some(role) = item.get("role").and_then(Value::as_str) else {
                return;
            };
            let role = if role == "developer" { "system" } else { role };
            let content = item
                .get("content")
                .map(content_to_chat)
                .unwrap_or(Value::Null);
            messages.push(json!({"role": role, "content": content}));
        }
    }
}

fn content_to_chat(content: &Value) -> Value {
    match content {
        Value::Array(parts) => Value::Array(parts.iter().filter_map(chat_part).collect()),
        other => other.clone(),
    }
}

fn chat_part(part: &Value) -> Option<Value> {
    match part.get("type").and_then(Value::as_str) {
        Some("input_text") | Some("output_text") | Some("text") => Some(json!({
            "type": "text",
            "text": part.get("text").cloned().unwrap_or_else(|| json!(""))
        })),
        Some("input_image") | Some("image_url") => {
            let url = image_url(part)?;
            Some(json!({"type": "image_url", "image_url": {"url": url}}))
        }
        Some("refusal") => Some(json!({
            "type": "text",
            "text": part.get("refusal").cloned().unwrap_or_else(|| json!(""))
        })),
        _ => None,
    }
}

fn image_url(part: &Value) -> Option<Value> {
    match part.get("image_url") {
        Some(Value::String(url)) => Some(Value::String(url.clone())),
        Some(value @ Value::Object(_)) => Some(
            value
                .get("url")
                .cloned()
                .unwrap_or_else(|| Value::String(String::new())),
        ),
        _ => part.get("url").cloned(),
    }
}

/// @cc [owner:ghuntley,label:proxy] responses-completion-translation
/// A non-streaming Chat Completions body MUST become a Responses object: the first choice's text,
/// reasoning, and tool calls MUST appear as `output` items, `finish_reason` `length` and
/// `content_filter` MUST map to `status: incomplete` with the matching `incomplete_details.reason`,
/// and an upstream `error` -- whether top-level or a choice-level `finish_reason: "error"` -- MUST
/// map to `status: failed` with an empty `output` and an `error` object.
pub fn completion_to_response(chat: &Value, model: &str, response_id: &str, created_at: i64) -> Value {
    if let Some(error) = chat.get("error").filter(|error| !error.is_null()) {
        return json!({
            "id": response_id,
            "object": "response",
            "created_at": created_at,
            "status": "failed",
            "model": model,
            "output": [],
            "error": normalize_error(error),
        });
    }
    let choice = chat
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first());
    let message = choice.and_then(|choice| choice.get("message"));
    let finish_reason = choice
        .and_then(|choice| choice.get("finish_reason"))
        .and_then(Value::as_str)
        .unwrap_or("stop");
    let (status, incomplete_reason) = match finish_reason {
        "length" => ("incomplete", Some("max_output_tokens")),
        "content_filter" => ("incomplete", Some("content_filter")),
        // A choice-level error finishes the same way the streaming translator treats it, so the two
        // paths agree on status for the same upstream body.
        "error" => ("failed", None),
        _ => ("completed", None),
    };

    let mut output = Vec::new();
    if let Some(reasoning) = message.and_then(reasoning_text) {
        output.push(reasoning_item("rs_0", &reasoning));
    }
    if let Some(text) = message.and_then(message_text).filter(|text| !text.is_empty()) {
        output.push(json!({
            "type": "message",
            "id": "msg_0",
            "role": "assistant",
            "status": "completed",
            "content": [{"type": "output_text", "text": text, "annotations": []}]
        }));
    }
    if let Some(tool_calls) = message.and_then(|message| message.get("tool_calls")).and_then(Value::as_array) {
        for (index, call) in tool_calls.iter().enumerate() {
            let call_id = call.get("id").and_then(Value::as_str).unwrap_or_default();
            let name = call
                .get("function")
                .and_then(|function| function.get("name"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            let arguments = call
                .get("function")
                .and_then(|function| function.get("arguments"))
                .map(arguments_text)
                .unwrap_or_default();
            output.push(json!({
                "type": "function_call",
                "id": format!("fc_{index}"),
                "call_id": call_id,
                "name": name,
                "arguments": arguments,
                "status": "completed"
            }));
        }
    }

    let mut response = json!({
        "id": response_id,
        "object": "response",
        "created_at": created_at,
        "status": status,
        "model": model,
        "output": output,
    });
    if let Some(usage) = chat.get("usage") {
        response["usage"] = usage_to_responses(usage);
    }
    if let Some(reason) = incomplete_reason {
        response["incomplete_details"] = json!({"reason": reason});
    }
    if status == "failed" {
        // A choice-level error carries no usable output; surface the error and drop partial items so
        // the non-streaming shape matches the streaming `response.failed` event.
        response["output"] = json!([]);
        let error = choice.and_then(|choice| choice.get("error")).cloned();
        response["error"] = match error {
            Some(error) if !error.is_null() => normalize_error(&error),
            _ => json!({
                "code": "upstream_error",
                "message": "upstream ended with an error finish reason",
            }),
        };
    }
    response
}

fn message_text(message: &Value) -> Option<String> {
    match message.get("content") {
        Some(Value::String(text)) => Some(text.clone()),
        Some(Value::Array(parts)) => {
            let text: String = parts
                .iter()
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect();
            Some(text)
        }
        _ => None,
    }
}

fn reasoning_text(message: &Value) -> Option<String> {
    message
        .get("reasoning")
        .or_else(|| message.get("reasoning_content"))
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

fn arguments_text(arguments: &Value) -> String {
    match arguments {
        Value::String(arguments) => arguments.clone(),
        other => other.to_string(),
    }
}

fn reasoning_item(item_id: &str, text: &str) -> Value {
    json!({
        "type": "reasoning",
        "id": item_id,
        "summary": [{"type": "summary_text", "text": text}]
    })
}

fn usage_to_responses(usage: &Value) -> Value {
    let number = |value: Option<&Value>| value.and_then(Value::as_i64).unwrap_or(0);
    let input = number(usage.get("prompt_tokens").or_else(|| usage.get("input_tokens")));
    let output = number(usage.get("completion_tokens").or_else(|| usage.get("output_tokens")));
    let total = number(usage.get("total_tokens")).max(input.saturating_add(output));
    let cached = number(
        usage
            .pointer("/prompt_tokens_details/cached_tokens")
            .or_else(|| usage.pointer("/input_tokens_details/cached_tokens")),
    );
    let reasoning = number(
        usage
            .pointer("/completion_tokens_details/reasoning_tokens")
            .or_else(|| usage.pointer("/output_tokens_details/reasoning_tokens")),
    );
    json!({
        "input_tokens": input,
        "input_tokens_details": {"cached_tokens": cached},
        "output_tokens": output,
        "output_tokens_details": {"reasoning_tokens": reasoning},
        "total_tokens": total,
    })
}

/// @cc [owner:ghuntley,label:proxy] responses-error-translation
/// An upstream error body MUST be returned in the Responses error shape when it carries an `error`
/// value; any other body, including non-JSON, MUST pass through byte-for-byte.
pub fn error_body(body: &str) -> String {
    let Ok(value) = serde_json::from_str::<Value>(body) else {
        return body.to_string();
    };
    match value.get("error") {
        Some(Value::String(message)) => {
            json!({"error": {"message": message, "type": "upstream_error"}}).to_string()
        }
        _ => body.to_string(),
    }
}

fn normalize_error(error: &Value) -> Value {
    match error {
        Value::String(message) => json!({"code": "upstream_error", "message": message}),
        Value::Object(map) => json!({
            "code": map
                .get("code")
                .or_else(|| map.get("type"))
                .cloned()
                .unwrap_or_else(|| json!("upstream_error")),
            "message": map
                .get("message")
                .cloned()
                .unwrap_or_else(|| json!("upstream error")),
        }),
        _ => json!({"code": "upstream_error", "message": "upstream error"}),
    }
}

#[derive(Clone, Debug)]
struct MessageState {
    output_index: usize,
    item_id: String,
    text: String,
}

#[derive(Clone, Debug)]
struct ReasoningState {
    output_index: usize,
    item_id: String,
    text: String,
}

#[derive(Clone, Debug)]
struct ToolState {
    output_index: usize,
    item_id: String,
    call_id: String,
    name: String,
    arguments: String,
}

#[derive(Clone, Debug)]
enum Finish {
    Completed,
    Length,
    ContentFilter,
    Failed(Value),
}

/// @cc [owner:ghuntley,label:proxy] responses-stream-single-terminal
/// `feed` MUST emit only complete SSE frames, buffering a partial trailing line until more bytes
/// arrive, and the translator MUST emit exactly one terminal event (`response.completed`,
/// `response.incomplete`, or `response.failed`) over the whole stream, including when EOF arrives
/// without a `[DONE]` sentinel.
pub struct ChatStreamTranslator {
    response_id: String,
    model: String,
    created_at: i64,
    buffer: Vec<u8>,
    started: bool,
    finished: bool,
    seq: u64,
    next_index: usize,
    message: Option<MessageState>,
    reasoning: Option<ReasoningState>,
    tools: std::collections::BTreeMap<u64, ToolState>,
    usage: Option<Value>,
    finish_reason: Option<String>,
}

impl ChatStreamTranslator {
    pub fn new(model: &str, response_id: &str, created_at: i64) -> Self {
        Self {
            response_id: response_id.to_string(),
            model: model.to_string(),
            created_at,
            buffer: Vec::new(),
            started: false,
            finished: false,
            seq: 0,
            next_index: 0,
            message: None,
            reasoning: None,
            tools: Default::default(),
            usage: None,
            finish_reason: None,
        }
    }

    /// Translates every complete `data:` line in `chunk`, returning response SSE frames.
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        self.buffer.extend_from_slice(chunk);
        while let Some(position) = self.buffer.iter().position(|byte| *byte == b'\n') {
            let mut line: Vec<u8> = self.buffer.drain(..=position).collect();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            self.process_line(&line, &mut out);
        }
        out
    }

    /// Flushes a trailing partial line and closes the response.
    pub fn finish_on_eof(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        if !self.buffer.is_empty() {
            let line = std::mem::take(&mut self.buffer);
            self.process_line(&line, &mut out);
        }
        if self.finished {
            return out;
        }
        let finish = match self.finish_reason.as_deref() {
            Some("length") => Finish::Length,
            Some("content_filter") => Finish::ContentFilter,
            Some("error") => Finish::Failed(json!({
                "code": "upstream_error",
                "message": "upstream ended with an error finish reason",
            })),
            Some(_) => Finish::Completed,
            None => Finish::Failed(json!({
                "code": "stream_truncated",
                "message": "upstream closed the stream before finishing",
            })),
        };
        self.finish(finish, &mut out);
        out
    }

    fn next_seq(&mut self) -> u64 {
        let seq = self.seq;
        self.seq += 1;
        seq
    }

    fn process_line(&mut self, line: &[u8], out: &mut Vec<u8>) {
        let Some(rest) = line.strip_prefix(b"data:") else {
            return;
        };
        let data = rest.strip_prefix(b" ").unwrap_or(rest);
        let data = data.trim_ascii();
        if data.is_empty() {
            return;
        }
        if data == b"[DONE]" {
            let finish = match self.finish_reason.as_deref() {
                Some("length") => Finish::Length,
                Some("content_filter") => Finish::ContentFilter,
                Some("error") => return,
                _ => Finish::Completed,
            };
            self.finish(finish, out);
            return;
        }
        let Ok(value) = serde_json::from_slice::<Value>(data) else {
            return;
        };
        self.process_chunk(&value, out);
    }

    fn process_chunk(&mut self, value: &Value, out: &mut Vec<u8>) {
        if self.finished {
            return;
        }
        self.ensure_started(out);
        if let Some(error) = value.get("error").filter(|error| !error.is_null()) {
            self.finish(Finish::Failed(normalize_error(error)), out);
            return;
        }
        let Some(choice) = value
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
        else {
            if let Some(usage) = value.get("usage") {
                self.usage = Some(usage.clone());
            }
            return;
        };
        if let Some(usage) = choice.get("usage").or_else(|| value.get("usage")) {
            self.usage = Some(usage.clone());
        }
        if choice.get("finish_reason").and_then(Value::as_str) == Some("error") {
            let error = choice
                .get("error")
                .cloned()
                .unwrap_or_else(|| json!("upstream error"));
            self.finish(Finish::Failed(normalize_error(&error)), out);
            return;
        }
        let delta = choice.get("delta");
        if let Some(text) = delta
            .and_then(|delta| delta.get("reasoning").or_else(|| delta.get("reasoning_content")))
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
        {
            self.reasoning_delta(text, out);
        }
        if let Some(text) = delta
            .and_then(|delta| delta.get("content"))
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
        {
            // An empty content delta (common in a first SSE chunk) must not create a message item;
            // the non-streaming path filters empty text the same way.
            self.text_delta(text, out);
        }
        if let Some(calls) = delta
            .and_then(|delta| delta.get("tool_calls"))
            .and_then(Value::as_array)
        {
            for call in calls {
                self.tool_delta(call, out);
            }
        }
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.finish_reason = Some(reason.to_string());
        }
    }

    fn ensure_started(&mut self, out: &mut Vec<u8>) {
        if self.started {
            return;
        }
        self.started = true;
        let seq = self.next_seq();
        emit(
            out,
            seq,
            "response.created",
            json!({
                "type": "response.created",
                "response": {
                    "id": self.response_id,
                    "object": "response",
                    "created_at": self.created_at,
                    "status": "in_progress",
                    "model": self.model,
                    "output": [],
                }
            }),
        );
    }

    fn text_delta(&mut self, text: &str, out: &mut Vec<u8>) {
        if self.message.is_none() {
            let output_index = self.take_index();
            let item_id = format!("msg_{output_index}");
            let seq = self.next_seq();
            emit(
                out,
                seq,
                "response.output_item.added",
                json!({
                    "type": "response.output_item.added",
                    "output_index": output_index,
                    "item": {
                        "type": "message",
                        "id": item_id,
                        "role": "assistant",
                        "status": "in_progress",
                        "content": [],
                    }
                }),
            );
            let seq = self.next_seq();
            emit(
                out,
                seq,
                "response.content_part.added",
                json!({
                    "type": "response.content_part.added",
                    "item_id": item_id,
                    "output_index": output_index,
                    "content_index": 0,
                    "part": {"type": "output_text", "text": "", "annotations": []},
                }),
            );
            self.message = Some(MessageState {
                output_index,
                item_id,
                text: String::new(),
            });
        }
        let seq = self.next_seq();
        let state = self.message.as_mut().expect("message state");
        state.text.push_str(text);
        let item_id = state.item_id.clone();
        let output_index = state.output_index;
        emit(
            out,
            seq,
            "response.output_text.delta",
            json!({
                "type": "response.output_text.delta",
                "item_id": item_id,
                "output_index": output_index,
                "content_index": 0,
                "delta": text,
            }),
        );
    }

    fn reasoning_delta(&mut self, text: &str, out: &mut Vec<u8>) {
        if self.reasoning.is_none() {
            let output_index = self.take_index();
            let item_id = format!("rs_{output_index}");
            let seq = self.next_seq();
            emit(
                out,
                seq,
                "response.output_item.added",
                json!({
                    "type": "response.output_item.added",
                    "output_index": output_index,
                    "item": {
                        "type": "reasoning",
                        "id": item_id,
                        "status": "in_progress",
                        "summary": [],
                    }
                }),
            );
            let seq = self.next_seq();
            emit(
                out,
                seq,
                "response.reasoning_summary_part.added",
                json!({
                    "type": "response.reasoning_summary_part.added",
                    "item_id": item_id,
                    "output_index": output_index,
                    "summary_index": 0,
                    "part": {"type": "summary_text", "text": ""},
                }),
            );
            self.reasoning = Some(ReasoningState {
                output_index,
                item_id,
                text: String::new(),
            });
        }
        let seq = self.next_seq();
        let state = self.reasoning.as_mut().expect("reasoning state");
        state.text.push_str(text);
        let item_id = state.item_id.clone();
        let output_index = state.output_index;
        emit(
            out,
            seq,
            "response.reasoning_summary_text.delta",
            json!({
                "type": "response.reasoning_summary_text.delta",
                "item_id": item_id,
                "output_index": output_index,
                "summary_index": 0,
                "delta": text,
            }),
        );
    }

    fn tool_delta(&mut self, call: &Value, out: &mut Vec<u8>) {
        let key = call.get("index").and_then(Value::as_u64).unwrap_or(0);
        if !self.tools.contains_key(&key) {
            let output_index = self.take_index();
            let item_id = format!("fc_{output_index}");
            let call_id = call
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let name = call
                .pointer("/function/name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let seq = self.next_seq();
            emit(
                out,
                seq,
                "response.output_item.added",
                json!({
                    "type": "response.output_item.added",
                    "output_index": output_index,
                    "item": {
                        "type": "function_call",
                        "id": item_id,
                        "call_id": call_id,
                        "name": name,
                        "arguments": "",
                        "status": "in_progress",
                    }
                }),
            );
            self.tools.insert(
                key,
                ToolState {
                    output_index,
                    item_id,
                    call_id,
                    name,
                    arguments: String::new(),
                },
            );
        }
        let mut emit_arguments: Option<String> = None;
        {
            let state = self.tools.get_mut(&key).expect("tool state");
            if let Some(call_id) = call
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
            {
                state.call_id = call_id.to_string();
            }
            if let Some(name) = call
                .pointer("/function/name")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
            {
                state.name = name.to_string();
            }
            if let Some(arguments) = call.pointer("/function/arguments").and_then(Value::as_str) {
                state.arguments.push_str(arguments);
                emit_arguments = Some(arguments.to_string());
            }
        }
        if let Some(arguments) = emit_arguments {
            let seq = self.next_seq();
            let state = self.tools.get(&key).expect("tool state");
            let item_id = state.item_id.clone();
            let output_index = state.output_index;
            emit(
                out,
                seq,
                "response.function_call_arguments.delta",
                json!({
                    "type": "response.function_call_arguments.delta",
                    "item_id": item_id,
                    "output_index": output_index,
                    "delta": arguments,
                }),
            );
        }
    }

    fn take_index(&mut self) -> usize {
        let index = self.next_index;
        self.next_index += 1;
        index
    }

    fn finish(&mut self, finish: Finish, out: &mut Vec<u8>) {
        if self.finished {
            return;
        }
        self.ensure_started(out);
        self.finished = true;
        let mut items: Vec<(usize, Value)> = Vec::new();

        if let Some(state) = self.reasoning.take() {
            let seq = self.next_seq();
            emit(
                out,
                seq,
                "response.reasoning_summary_text.done",
                json!({
                    "type": "response.reasoning_summary_text.done",
                    "item_id": state.item_id,
                    "output_index": state.output_index,
                    "summary_index": 0,
                    "text": state.text,
                }),
            );
            let seq = self.next_seq();
            emit(
                out,
                seq,
                "response.reasoning_summary_part.done",
                json!({
                    "type": "response.reasoning_summary_part.done",
                    "item_id": state.item_id,
                    "output_index": state.output_index,
                    "summary_index": 0,
                    "part": {"type": "summary_text", "text": state.text},
                }),
            );
            let item = reasoning_item(&state.item_id, &state.text);
            let seq = self.next_seq();
            emit(
                out,
                seq,
                "response.output_item.done",
                json!({
                    "type": "response.output_item.done",
                    "output_index": state.output_index,
                    "item": item,
                }),
            );
            items.push((state.output_index, item));
        }

        if let Some(state) = self.message.take() {
            let seq = self.next_seq();
            emit(
                out,
                seq,
                "response.output_text.done",
                json!({
                    "type": "response.output_text.done",
                    "item_id": state.item_id,
                    "output_index": state.output_index,
                    "content_index": 0,
                    "text": state.text,
                }),
            );
            let seq = self.next_seq();
            emit(
                out,
                seq,
                "response.content_part.done",
                json!({
                    "type": "response.content_part.done",
                    "item_id": state.item_id,
                    "output_index": state.output_index,
                    "content_index": 0,
                    "part": {"type": "output_text", "text": state.text, "annotations": []},
                }),
            );
            let item = json!({
                "type": "message",
                "id": state.item_id,
                "role": "assistant",
                "status": "completed",
                "content": [{"type": "output_text", "text": state.text, "annotations": []}]
            });
            let seq = self.next_seq();
            emit(
                out,
                seq,
                "response.output_item.done",
                json!({
                    "type": "response.output_item.done",
                    "output_index": state.output_index,
                    "item": item,
                }),
            );
            items.push((state.output_index, item));
        }

        for state in std::mem::take(&mut self.tools).into_values() {
            let seq = self.next_seq();
            emit(
                out,
                seq,
                "response.function_call_arguments.done",
                json!({
                    "type": "response.function_call_arguments.done",
                    "item_id": state.item_id,
                    "output_index": state.output_index,
                    "arguments": state.arguments,
                }),
            );
            let item = json!({
                "type": "function_call",
                "id": state.item_id,
                "call_id": state.call_id,
                "name": state.name,
                "arguments": state.arguments,
                "status": "completed"
            });
            let seq = self.next_seq();
            emit(
                out,
                seq,
                "response.output_item.done",
                json!({
                    "type": "response.output_item.done",
                    "output_index": state.output_index,
                    "item": item,
                }),
            );
            items.push((state.output_index, item));
        }

        items.sort_by_key(|(index, _)| *index);
        let mut output: Vec<Value> = items.into_iter().map(|(_, item)| item).collect();
        let (event, status, incomplete_reason) = match &finish {
            Finish::Completed => ("response.completed", "completed", None),
            Finish::Length => ("response.incomplete", "incomplete", Some("max_output_tokens")),
            Finish::ContentFilter => ("response.incomplete", "incomplete", Some("content_filter")),
            Finish::Failed(_) => {
                // A failed response carries no usable output; this matches the non-streaming
                // `completion_to_response` shape so both transports report the same body.
                output.clear();
                ("response.failed", "failed", None)
            }
        };
        let mut response = json!({
            "id": self.response_id,
            "object": "response",
            "created_at": self.created_at,
            "status": status,
            "model": self.model,
            "output": output,
        });
        if let Some(usage) = &self.usage {
            response["usage"] = usage_to_responses(usage);
        }
        if let Some(reason) = incomplete_reason {
            response["incomplete_details"] = json!({"reason": reason});
        }
        if let Finish::Failed(error) = finish {
            response["error"] = error;
        }
        let seq = self.next_seq();
        emit(out, seq, event, json!({"type": event, "response": response}));
    }
}

fn emit(out: &mut Vec<u8>, seq: u64, event: &str, mut data: Value) {
    if let Some(object) = data.as_object_mut() {
        object.insert("sequence_number".into(), json!(seq));
    }
    let _ = writeln!(out, "event: {event}");
    let _ = writeln!(out, "data: {data}\n");
}


#[cfg(test)]
mod tests {
    use super::*;

    fn stream_frames(frames: &[u8]) -> Vec<Value> {
        let text = String::from_utf8(frames.to_vec()).unwrap();
        text.lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .filter_map(|data| serde_json::from_str::<Value>(data).ok())
            .collect()
    }

    #[test]
    fn request_translation_maps_text_instructions_and_limits() {
        let request = json!({
            "model": "cline/anthropic/claude-sonnet-4-6",
            "instructions": "be terse",
            "input": [
                {"role": "user", "content": [{"type": "input_text", "text": "hello"}]},
                {"role": "assistant", "content": [{"type": "output_text", "text": "hi"}]}
            ],
            "max_output_tokens": 512,
            "stream": true,
            "reasoning": {"effort": "high"}
        });
        let chat = request_to_chat(&request);
        assert_eq!(chat["model"], "cline/anthropic/claude-sonnet-4-6");
        assert_eq!(chat["messages"][0], json!({"role": "system", "content": "be terse"}));
        assert_eq!(
            chat["messages"][1],
            json!({"role": "user", "content": [{"type": "text", "text": "hello"}]})
        );
        assert_eq!(
            chat["messages"][2],
            json!({"role": "assistant", "content": [{"type": "text", "text": "hi"}]})
        );
        assert_eq!(chat["max_tokens"], 512);
        assert_eq!(chat["reasoning_effort"], "high");
        assert_eq!(chat["stream"], true);
    }

    #[test]
    fn request_translation_accepts_plain_string_input_and_images() {
        let chat = request_to_chat(&json!({"input": "plain"}));
        assert_eq!(chat["messages"], json!([{"role": "user", "content": "plain"}]));
        let chat = request_to_chat(&json!({"input": [{"role": "user", "content": [
            {"type": "input_image", "image_url": "data:image/png;base64,xx"}
        ]}]}));
        assert_eq!(
            chat["messages"][0]["content"][0],
            json!({"type": "image_url", "image_url": {"url": "data:image/png;base64,xx"}})
        );
    }

    #[test]
    fn request_translation_flattens_tools_and_tool_choice() {
        let chat = request_to_chat(&json!({
            "input": "weather?",
            "tools": [
                {"type": "function", "name": "get_weather", "description": "d",
                 "parameters": {"type": "object", "properties": {}}},
                {"type": "web_search"}
            ],
            "tool_choice": {"type": "function", "name": "get_weather"}
        }));
        assert_eq!(
            chat["tools"],
            json!([{"type": "function", "function": {
                "name": "get_weather",
                "description": "d",
                "parameters": {"type": "object", "properties": {}}
            }}])
        );
        assert_eq!(
            chat["tool_choice"],
            json!({"type": "function", "function": {"name": "get_weather"}})
        );
    }

    #[test]
    fn null_tool_fields_fall_back_to_defaults() {
        let chat = request_to_chat(&json!({
            "input": "go",
            "tools": [{"type": "function", "name": "ping", "description": null, "parameters": null}],
        }));
        let function = chat["tools"][0]["function"].clone();
        assert_eq!(function["name"], "ping");
        assert!(function.get("description").is_none(), "null description must be omitted");
        assert_eq!(function["parameters"], json!({"type": "object", "properties": {}}));
    }

    #[test]
    fn strict_flag_survives_tool_translation() {
        let chat = request_to_chat(&json!({
            "input": "go",
            "tools": [{
                "type": "function",
                "name": "lookup",
                "strict": true,
                "parameters": {"type": "object", "properties": {}},
            }],
        }));
        assert_eq!(
            chat["tools"][0]["function"]["strict"],
            json!(true),
            "dropping strict would relax the caller's schema contract"
        );
        // A non-boolean strict is not a schema contract; forward nothing rather than a bad value.
        let malformed = request_to_chat(&json!({
            "input": "go",
            "tools": [{"type": "function", "name": "lookup", "strict": "yes"}],
        }));
        assert!(malformed["tools"][0]["function"].get("strict").is_none());
    }

    #[test]
    fn failed_stream_clears_partial_output() {
        let mut translator = ChatStreamTranslator::new("m", "resp_1", 0);
        let mut out = translator.feed(
            b"data: {\"choices\":[{\"delta\":{\"content\":\"partial\"},\"finish_reason\":\"error\",\"error\":{\"code\":\"boom\"}}]}\n\n",
        );
        out.extend(translator.finish_on_eof());
        let frames = stream_frames(&out);
        let failed = frames.iter().find(|f| f["type"] == "response.failed").unwrap();
        assert_eq!(
            failed["response"]["output"],
            json!([]),
            "a failed response must not expose partial output, matching the non-streaming shape"
        );
        assert!(failed["response"].get("error").is_some());
    }

    #[test]
    fn empty_content_delta_creates_no_message_item() {
        let mut translator = ChatStreamTranslator::new("m", "resp_1", 0);
        let mut out = translator.feed(
            b"data: {\"choices\":[{\"delta\":{\"content\":\"\"}}]}\n\ndata: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"function\":{\"name\":\"f\",\"arguments\":\"{}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\ndata: [DONE]\n\n",
        );
        out.extend(translator.finish_on_eof());
        let frames = stream_frames(&out);
        assert!(
            !frames.iter().any(|f| f.get("type").and_then(|v| v.as_str()) == Some("response.output_item.added")
                && f["item"]["type"] == "message"),
            "an empty content delta must not add a message item"
        );
        let completed = frames.iter().find(|f| f["type"] == "response.completed").unwrap();
        assert!(
            !completed["response"]["output"]
                .as_array()
                .unwrap()
                .iter()
                .any(|i| i["type"] == "message"),
            "a tool-only stream must have no message output item"
        );
    }

    #[test]
    fn null_optional_fields_are_omitted() {
        let chat = request_to_chat(&json!({
            "model": "m",
            "reasoning": {"effort": null},
            "text": {"format": {"type": "json_schema"}},
            "input": [{"role": "user", "content": "hi"}],
        }));
        assert!(chat.get("reasoning_effort").is_none(), "null effort must be omitted");
        let schema = chat["response_format"]["json_schema"].clone();
        assert_eq!(schema["name"], "response");
        assert_eq!(schema["schema"], json!({}));
        assert!(schema.get("strict").is_none(), "null strict must be omitted");
    }

    #[test]
    fn explicit_nulls_in_json_schema_fields_fall_back_to_defaults() {
        let chat = request_to_chat(&json!({
            "model": "m",
            "text": {"format": {"type": "json_schema", "name": null, "schema": null, "strict": null}},
            "input": [{"role": "user", "content": "hi"}],
        }));
        let schema = chat["response_format"]["json_schema"].clone();
        assert_eq!(schema["name"], "response");
        assert_eq!(schema["schema"], json!({}));
        assert!(schema.get("strict").is_none());
    }

    #[test]
    fn choice_level_error_maps_to_failed() {
        let chat = json!({
            "choices": [{
                "finish_reason": "error",
                "error": {"code": "rate_limit", "message": "slow down"},
                "message": {"content": "partial"},
            }],
        });
        let response = completion_to_response(&chat, "m", "resp_3", 7);
        assert_eq!(response["status"], "failed");
        assert_eq!(response["output"], json!([]), "failed output must be empty");
        assert_eq!(response["error"]["code"], "rate_limit");
    }

    #[test]
    fn unrecognized_finish_reason_still_completes() {
        let chat = json!({
            "choices": [{"finish_reason": "stop", "message": {"content": "done"}}],
        });
        let response = completion_to_response(&chat, "m", "resp_4", 7);
        assert_eq!(response["status"], "completed");
        assert!(response.get("error").is_none());
    }

    #[test]
    fn tool_choice_is_dropped_without_function_tools() {
        let hosted_only = request_to_chat(&json!({
            "input": "search",
            "tools": [{"type": "web_search"}, {"type": "file_search"}],
            "tool_choice": "auto"
        }));
        assert!(hosted_only.get("tools").is_none(), "hosted tools must not convert");
        assert!(
            hosted_only.get("tool_choice").is_none(),
            "tool_choice without tools is rejected upstream"
        );

        let no_tools = request_to_chat(&json!({"input": "hi", "tool_choice": "required"}));
        assert!(no_tools.get("tool_choice").is_none());

        let null_choice = request_to_chat(&json!({
            "input": "hi",
            "tools": [{"type": "function", "name": "f", "parameters": {"type": "object"}}],
            "tool_choice": null
        }));
        assert!(null_choice.get("tools").is_some());
        assert!(null_choice.get("tool_choice").is_none(), "null must stay omitted");
    }

    #[test]
    fn request_translation_keeps_function_call_history() {
        let chat = request_to_chat(&json!({"input": [
            {"type": "function_call", "call_id": "call_1", "name": "get_weather", "arguments": "{\"city\":\"x\"}"},
            {"type": "function_call_output", "call_id": "call_1", "output": "sunny"}
        ]}));
        assert_eq!(
            chat["messages"][0],
            json!({"role": "assistant", "content": null, "tool_calls": [{
                "id": "call_1", "type": "function",
                "function": {"name": "get_weather", "arguments": "{\"city\":\"x\"}"}
            }]})
        );
        assert_eq!(
            chat["messages"][1],
            json!({"role": "tool", "tool_call_id": "call_1", "content": "sunny"})
        );
    }

    #[test]
    fn non_stream_completion_becomes_a_response_object() {
        let chat = json!({
            "id": "gen-1",
            "choices": [{"message": {"role": "assistant", "content": "Paris."}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 14, "completion_tokens": 8, "total_tokens": 22}
        });
        let response = completion_to_response(&chat, "cline/anthropic/claude-sonnet-4-6", "resp_1", 5);
        assert_eq!(response["status"], "completed");
        assert_eq!(response["output"][0]["content"][0]["text"], "Paris.");
        assert_eq!(response["usage"]["input_tokens"], 14);
        assert_eq!(response["usage"]["output_tokens"], 8);
        assert_eq!(response["usage"]["total_tokens"], 22);
    }

    #[test]
    fn non_stream_completion_maps_tool_calls_and_truncation() {
        let chat = json!({
            "choices": [{
                "message": {"role": "assistant", "content": null, "tool_calls": [{
                    "id": "call_9", "type": "function",
                    "function": {"name": "get_weather", "arguments": "{\"city\":\"x\"}"}
                }]},
                "finish_reason": "length"
            }]
        });
        let response = completion_to_response(&chat, "m", "resp_2", 5);
        assert_eq!(response["status"], "incomplete");
        assert_eq!(response["incomplete_details"]["reason"], "max_output_tokens");
        assert_eq!(response["output"][0]["type"], "function_call");
        assert_eq!(response["output"][0]["call_id"], "call_9");
    }

    #[test]
    fn stream_translation_emits_created_deltas_and_completed() {
        let mut translator = ChatStreamTranslator::new("m", "resp_1", 5);
        let mut out = translator.feed(b"data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\n");
        assert!(!out.is_empty());
        out.extend(translator.feed(
            b"data: {\"choices\":[{\"delta\":{\"content\":\"lo\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":2}}\n\n",
        ));
        out.extend(translator.feed(b"data: [DONE]\n\n"));
        let frames = stream_frames(&out);
        let kinds: Vec<&str> = frames.iter().map(|f| f["type"].as_str().unwrap()).collect();
        assert_eq!(kinds[0], "response.created");
        assert!(kinds.contains(&"response.output_text.delta"));
        assert_eq!(*kinds.last().unwrap(), "response.completed");
        let completed = frames.last().unwrap();
        assert_eq!(completed["response"]["output"][0]["content"][0]["text"], "Hello");
        assert_eq!(completed["response"]["usage"]["input_tokens"], 1);
    }

    #[test]
    fn stream_translation_handles_split_lines_reasoning_and_tools() {
        let mut translator = ChatStreamTranslator::new("m", "resp_1", 5);
        let mut out = translator.feed(b"data: {\"choices\":[{\"delta\":{\"reasoning\":\"think\",\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"f\",\"arguments\":\"{\\\"a\\\":\"}}]}}]}\n\ndata: {\"cho");
        out.extend(translator.feed(
            b"ices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"1}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\ndata: [DONE]\n\n",
        ));
        let frames = stream_frames(&out);
        let kinds: Vec<&str> = frames.iter().map(|f| f["type"].as_str().unwrap()).collect();
        assert!(kinds.contains(&"response.reasoning_summary_text.delta"));
        assert!(kinds.contains(&"response.function_call_arguments.delta"));
        assert!(kinds.contains(&"response.function_call_arguments.done"));
        let completed = frames.last().unwrap();
        assert_eq!(completed["type"], "response.completed");
        let output = completed["response"]["output"].as_array().unwrap();
        let call = output.iter().find(|item| item["type"] == "function_call").unwrap();
        assert_eq!(call["arguments"], "{\"a\":1}");
    }

    #[test]
    fn stream_translation_reports_failures_and_truncation() {
        let mut translator = ChatStreamTranslator::new("m", "resp_1", 5);
        let out = translator.feed(
            b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"error\",\"error\":{\"code\":\"rate_limit\",\"message\":\"slow\"}}]}\n\n",
        );
        let frames = stream_frames(&out);
        assert_eq!(frames.last().unwrap()["type"], "response.failed");
        assert_eq!(frames.last().unwrap()["response"]["error"]["code"], "rate_limit");

        let mut truncated = ChatStreamTranslator::new("m", "resp_2", 5);
        let mut out = truncated.feed(b"data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n");
        out.extend(truncated.finish_on_eof());
        let frames = stream_frames(&out);
        assert!(!frames.iter().any(|frame| frame["type"] == "response.completed"));
    }

    #[test]
    fn error_bodies_are_converted_to_the_responses_shape() {
        let converted = error_body(r#"{"error":"Unauthorized: bad key"}"#);
        assert_eq!(
            serde_json::from_str::<Value>(&converted).unwrap(),
            json!({"error": {"message": "Unauthorized: bad key", "type": "upstream_error"}})
        );
        let untouched = r#"{"error":{"code":"x","message":"y"}}"#;
        assert_eq!(error_body(untouched), untouched);
    }
}
