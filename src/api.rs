use crate::config::{Context, Outcome, truncate_middle};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::thread;
use std::time::Duration;

const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const API_VERSION: &str = "2026-10-21";
const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";
const MAX_ATTEMPTS: u32 = 6;

pub struct Client {
    api_key: String,
    pub model: String,
    effort: String,
    fallbacks: bool,
    http: reqwest::blocking::Client,
    url: String,
}

pub struct Response {
    pub content: Vec<Value>,
    pub stop_reason: String,
    pub stop_details: Value,
    pub usage: Value,
    pub invalid_tool_ids: Vec<String>,
    pub served_model: String,
}

enum Failure {
    Fatal(String),
    Retryable(String, Option<Duration>),
}

#[derive(Default)]
struct StreamState {
    blocks: Vec<Value>,
    buffers: Vec<String>,
    stop_reason: String,
    stop_details: Value,
    usage: Value,
    model: String,
}

pub fn new_client(api_key: String, model: &str, effort: &str, fallbacks: bool) -> Outcome<Client> {
    let builder = reqwest::blocking::Client::builder().timeout(None).connect_timeout(Duration::from_secs(30));
    let http = builder.build().context("http client")?;
    let base_url = std::env::var("ANTHROPIC_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.into());
    let url = format!("{}/v1/messages", base_url.trim_end_matches('/'));
    Ok(Client { api_key, model: model.into(), effort: effort.into(), fallbacks, http, url })
}

fn request_body(client: &Client, system: &Value, tools: &Value, messages: &[Value]) -> Value {
    let mut body = json!({
        "model": client.model,
        "max_tokens": 64000,
        "stream": true,
        "system": system,
        "tools": tools,
        "messages": messages,
        "output_config": {"effort": client.effort},
        "cache_control": {"type": "ephemeral"},
    });
    if client.fallbacks {
        body["fallbacks"] = json!("default");
    }
    body
}

pub fn call(client: &Client, system: &Value, tools: &Value, messages: &[Value]) -> Outcome<Response> {
    let body = request_body(client, system, tools, messages).to_string();
    let mut last_error = String::new();
    for attempt in 0..MAX_ATTEMPTS {
        let suggested_wait = match send_once(client, &body) {
            Ok(response) => return Ok(response),
            Err(Failure::Fatal(error)) => return Err(error),
            Err(Failure::Retryable(error, wait)) => {
                last_error = error;
                wait
            }
        };
        let backoff = Duration::from_secs(2u64.pow(attempt + 1));
        let wait = suggested_wait.unwrap_or(backoff).min(Duration::from_secs(120));
        eprintln!("\napi retry {} in {}s: {}", attempt + 1, wait.as_secs(), truncate_middle(&last_error, 300));
        thread::sleep(wait);
    }
    Err(format!("api failed after retries: {last_error}"))
}

fn send_once(client: &Client, body: &str) -> Result<Response, Failure> {
    let mut request = client.http.post(&client.url).header("x-api-key", &client.api_key);
    request = request.header("anthropic-version", API_VERSION).header("content-type", "application/json");
    if client.fallbacks {
        request = request.header("anthropic-beta", FALLBACK_BETA);
    }
    let response = request.body(body.to_string()).send().map_err(|error| Failure::Retryable(error.to_string(), None))?;
    let status = response.status().as_u16();
    if status == 200 {
        return read_stream(BufReader::new(response));
    }
    let retry_after = response.headers().get("retry-after").and_then(|value| value.to_str().ok()?.parse().ok());
    let error = format!("HTTP {status}: {}", truncate_middle(&response.text().unwrap_or_default(), 2000));
    if [408, 409, 429].contains(&status) || status >= 500 {
        return Err(Failure::Retryable(error, retry_after.map(Duration::from_secs)));
    }
    Err(Failure::Fatal(error))
}

fn read_stream<R: BufRead>(reader: R) -> Result<Response, Failure> {
    let mut state = StreamState::default();
    for line in reader.lines() {
        let line = line.map_err(|error| Failure::Retryable(format!("stream: {error}"), None))?;
        let Some(data) = line.strip_prefix("data:") else { continue };
        let event: Value = serde_json::from_str(data.trim()).map_err(|error| Failure::Fatal(format!("bad event json: {error}")))?;
        handle_event(&mut state, &event)?;
    }
    if state.stop_reason.is_empty() {
        return Err(Failure::Retryable("stream ended before message_delta".into(), None));
    }
    Ok(finish(state))
}

fn handle_event(state: &mut StreamState, event: &Value) -> Result<(), Failure> {
    let index = event["index"].as_u64().unwrap_or(0) as usize;
    match event["type"].as_str().unwrap_or("") {
        "message_start" => on_message_start(state, &event["message"]),
        "content_block_start" => start_block(state, index, event["content_block"].clone()),
        "content_block_delta" => apply_delta(state, index, &event["delta"]),
        "message_delta" => on_message_delta(state, event),
        "error" => return Err(classify_error(&event["error"])),
        _ => {}
    }
    Ok(())
}

fn on_message_start(state: &mut StreamState, message: &Value) {
    state.usage = message["usage"].clone();
    state.model = message["model"].as_str().unwrap_or("").into();
}

fn classify_error(error: &Value) -> Failure {
    let description = format!("stream error: {error}");
    match error["type"].as_str().unwrap_or("") {
        "overloaded_error" | "api_error" | "rate_limit_error" => Failure::Retryable(description, None),
        _ => Failure::Fatal(description),
    }
}

fn start_block(state: &mut StreamState, index: usize, block: Value) {
    while state.blocks.len() <= index {
        state.blocks.push(Value::Null);
        state.buffers.push(String::new());
    }
    if block["type"] == "fallback" {
        eprintln!("\n[fallback: {} -> {}]", block["from"]["model"], block["to"]["model"]);
    }
    state.blocks[index] = block;
}

fn apply_delta(state: &mut StreamState, index: usize, delta: &Value) {
    if index >= state.blocks.len() {
        return;
    }
    let field = |name: &str| delta[name].as_str().unwrap_or("").to_string();
    match delta["type"].as_str().unwrap_or("") {
        "text_delta" => {
            let text = field("text");
            eprint!("{text}");
            let _ = std::io::stderr().flush();
            state.buffers[index].push_str(&text);
        }
        "thinking_delta" => state.buffers[index].push_str(&field("thinking")),
        "input_json_delta" => state.buffers[index].push_str(&field("partial_json")),
        "signature_delta" => state.blocks[index]["signature"] = Value::String(field("signature")),
        _ => {}
    }
}

fn on_message_delta(state: &mut StreamState, event: &Value) {
    if let Some(reason) = event["delta"]["stop_reason"].as_str() {
        state.stop_reason = reason.into();
    }
    if !event["delta"]["stop_details"].is_null() {
        state.stop_details = event["delta"]["stop_details"].clone();
    }
    let (Some(update), Some(usage)) = (event["usage"].as_object(), state.usage.as_object_mut()) else { return };
    for (key, value) in update {
        usage.insert(key.clone(), value.clone());
    }
}

fn finish(state: StreamState) -> Response {
    let mut invalid_tool_ids = Vec::new();
    let mut content = Vec::new();
    for (mut block, buffer) in state.blocks.into_iter().zip(state.buffers) {
        if block.is_null() {
            continue;
        }
        finalise_block(&mut block, buffer, &mut invalid_tool_ids);
        content.push(block);
    }
    let (stop_reason, stop_details, usage, served_model) = (state.stop_reason, state.stop_details, state.usage, state.model);
    Response { content, stop_reason, stop_details, usage, invalid_tool_ids, served_model }
}

fn finalise_block(block: &mut Value, buffer: String, invalid_tool_ids: &mut Vec<String>) {
    let block_type = block["type"].as_str().unwrap_or("").to_string();
    match block_type.as_str() {
        "text" => block["text"] = Value::String(buffer),
        "thinking" => block["thinking"] = Value::String(buffer),
        "tool_use" => {
            let input = parse_tool_input(block, &buffer, invalid_tool_ids);
            block["input"] = input;
        }
        _ => {}
    }
}

fn parse_tool_input(block: &Value, buffer: &str, invalid_tool_ids: &mut Vec<String>) -> Value {
    if buffer.trim().is_empty() {
        return json!({});
    }
    match serde_json::from_str::<Value>(buffer) {
        Ok(input) if input.is_object() => input,
        _ => {
            invalid_tool_ids.push(block["id"].as_str().unwrap_or("").into());
            json!({})
        }
    }
}

pub fn strip_before_fallback(content: Vec<Value>) -> Vec<Value> {
    let Some(boundary) = content.iter().rposition(|block| block["type"] == "fallback") else { return content };
    let keep = |index: usize, block: &Value| index >= boundary || block["type"] == "text" || block["type"] == "fallback";
    content.into_iter().enumerate().filter(|(index, block)| keep(*index, block)).map(|(_, block)| block).collect()
}
