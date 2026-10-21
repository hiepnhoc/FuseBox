//! Kiro (AWS): `GenerateAssistantResponse` on the CodeWhisperer data plane. The IR
//! is turned into Kiro's conversation state, and the AWS event-stream frames that
//! come back into IR events. Sign-in is AWS IAM Identity Center (PKCE) or AWS
//! Builder ID (device code); credits come from `getUsageLimits`.
//!
//! The wire format follows what the Kiro IDE and kiro-cli send (captured), and
//! the open Kiro gateways that have run it in production.

use std::collections::HashMap;
use std::pin::Pin;

use anyhow::{Context, Result, anyhow, bail};
use bytes::{Buf, BytesMut};
use chrono::{DateTime, Utc};
use futures::{Stream, StreamExt};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::accounts::{Credential, OAuth};
use crate::device::Signed;
use crate::ir::{Event, Finish, Image, Part, Request, Role, Usage};
use crate::quota::Window;
use crate::upstream::{Prepared, Target};

/// Where sign-ins go by default; the data plane follows the profile.
pub const DEFAULT_REGION: &str = "us-east-1";
const BUILDER_ID_START: &str = "https://view.awsapps.com/start";
const SOCIAL_REFRESH: &str = "https://prod.us-east-1.auth.desktop.kiro.dev/refreshToken";
const SCOPES: &[&str] = &[
    "codewhisperer:completions",
    "codewhisperer:analysis",
    "codewhisperer:conversations",
    "codewhisperer:transformations",
    "codewhisperer:taskassist",
];
const KIRO_VERSION: &str = "0.11.107";
const NODE_VERSION: &str = "22.22.0";
const STREAM_SDK: &str = "1.0.34";
const RUNTIME_SDK: &str = "1.0.0";
const DEFAULT_MAX_TOKENS: u64 = 32_000;
/// Kiro refuses longer tool descriptions.
const MAX_TOOL_DESCRIPTION: usize = 10_237;
const PRIMING_REPLY: &str = "I will follow these instructions.";
const MAX_FRAME: usize = 16 << 20;
/// Models whose weekly (monthly here) credits renew; the dashboard knows this name.
pub const WINDOW: &str = "month";

// ------------------------------------------------------------------ addresses

fn oidc(region: &str) -> String {
    format!("https://oidc.{region}.amazonaws.com")
}

/// The CodeWhisperer data plane for a profile's region.
pub fn data_plane(region: &str) -> String {
    format!("https://q.{region}.amazonaws.com")
}

/// `arn:aws:codewhisperer:{region}:{account}:profile/{id}`.
fn arn_region(arn: &str) -> Option<&str> {
    let mut parts = arn.split(':');
    (parts.next()? == "arn" && parts.next()? == "aws" && parts.next()? == "codewhisperer")
        .then(|| parts.next())
        .flatten()
        .filter(|r| valid_region(r))
}

/// Only shapes like `us-east-1` may be put in a hostname.
pub fn valid_region(r: &str) -> bool {
    let mut parts = r.split('-');
    let (Some(a), Some(b), Some(c), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    a.len() == 2
        && a.chars().all(|c| c.is_ascii_lowercase())
        && !b.is_empty()
        && b.chars().all(|c| c.is_ascii_lowercase())
        && (1..=2).contains(&c.len())
        && c.chars().all(|c| c.is_ascii_digit())
}

/// What a request needs from the credential file.
struct Creds {
    token: String,
    profile_arn: Option<String>,
    region: String,
    machine_id: String,
}

fn creds(o: &OAuth, device_id: &str) -> Creds {
    let profile_arn = o.field("profile_arn").map(String::from);
    let region = profile_arn
        .as_deref()
        .and_then(arn_region)
        .map(String::from)
        .or_else(|| o.field("profile_region").filter(|r| valid_region(r)).map(String::from))
        .unwrap_or_else(|| DEFAULT_REGION.into());
    Creds {
        token: o.access_token.clone(),
        profile_arn,
        region,
        machine_id: o.field("machine_id").unwrap_or(device_id).to_string(),
    }
}

fn system_version() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin#24.6.0",
        "windows" => "win32#10.0.22631",
        _ => "linux#6.6.87",
    }
}

fn user_agent(api: &str, sdk: &str, flags: &str, machine: &str) -> String {
    format!(
        "aws-sdk-js/{sdk} ua/2.1 os/{} lang/js md/nodejs#{NODE_VERSION} api/{api}#{sdk} m/{flags} KiroIDE-{KIRO_VERSION}-{machine}",
        system_version()
    )
}

fn short_agent(sdk: &str, machine: &str) -> String {
    format!("aws-sdk-js/{sdk} KiroIDE-{KIRO_VERSION}-{machine}")
}

/// Headers of the REST calls (profiles, models, usage).
fn runtime_headers(token: &str, machine: &str) -> Vec<(String, String)> {
    vec![
        ("authorization".into(), format!("Bearer {token}")),
        ("accept".into(), "application/json".into()),
        ("user-agent".into(), user_agent("codewhispererruntime", RUNTIME_SDK, "N,E", machine)),
        ("x-amz-user-agent".into(), short_agent(RUNTIME_SDK, machine)),
        ("x-amzn-codewhisperer-optout".into(), "true".into()),
        ("amz-sdk-invocation-id".into(), uuid::Uuid::new_v4().to_string()),
        ("amz-sdk-request".into(), "attempt=1; max=2".into()),
    ]
}

fn with_headers(mut rb: reqwest::RequestBuilder, headers: &[(String, String)]) -> reqwest::RequestBuilder {
    for (k, v) in headers {
        rb = rb.header(k.as_str(), v.as_str());
    }
    rb
}

// --------------------------------------------------------------------- models

/// The Kiro model id for a client's model name: Fusebox and Claude name versions
/// with dashes (`claude-opus-4-8`), Kiro with a dot (`claude-opus-4.8`); a trailing
/// `-thinking` stays for `build_request`.
pub fn model_id(model: &str) -> String {
    let lower = model.trim().to_ascii_lowercase();
    let lower = lower.trim_end_matches("[1m]");
    let (base, thinking) = match lower.strip_suffix("-thinking") {
        Some(b) => (b, true),
        None => (lower, false),
    };
    let parts: Vec<&str> = base.split('-').collect();
    let digits = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit());
    let mut out = match parts.as_slice() {
        ["claude", kind, major, minor, rest @ ..]
            if ["opus", "sonnet", "haiku"].contains(kind) && digits(major) && digits(minor) =>
        {
            // A date suffix (`claude-haiku-4-5-20251001`) names Anthropic's snapshot, not Kiro's.
            let rest: Vec<&str> = rest.iter().copied().filter(|r| !(r.len() == 8 && digits(r))).collect();
            let mut s = format!("claude-{kind}-{major}.{minor}");
            for r in rest {
                s.push('-');
                s.push_str(r);
            }
            s
        }
        ["gpt", major, minor, rest @ ..] if digits(major) && digits(minor) => {
            let mut s = format!("gpt-{major}.{minor}");
            for r in rest {
                s.push('-');
                s.push_str(r);
            }
            s
        }
        _ => base.to_string(),
    };
    if thinking {
        out.push_str("-thinking");
    }
    out
}

/// A Kiro model id as Fusebox names it, so Claude and GPT names route to their vendor first.
pub fn public_name(model_id: &str) -> String {
    let lower = model_id.trim().to_ascii_lowercase();
    let parts: Vec<&str> = lower.splitn(3, '-').collect();
    match parts.as_slice() {
        ["claude", kind, version] => {
            let (ver, rest) = version.split_once('-').map(|(v, r)| (v, Some(r))).unwrap_or((version, None));
            if let Some((major, minor)) = ver.split_once('.')
                && major.chars().all(|c| c.is_ascii_digit())
                && minor.chars().all(|c| c.is_ascii_digit())
            {
                let mut s = format!("claude-{kind}-{major}-{minor}");
                if let Some(r) = rest {
                    s.push('-');
                    s.push_str(r);
                }
                return s;
            }
            lower
        }
        _ => lower,
    }
}

/// Kiro answers 400 to `additionalModelRequestFields` on models without an effort knob.
fn supports_effort(model_id: &str) -> Option<&'static str> {
    let m = model_id.trim_end_matches("-thinking");
    let version = |prefix: &str| m.strip_prefix(prefix).and_then(|v| v.split('-').next()).unwrap_or_default();
    let major_minor = |v: &str| {
        let (a, b) = v.split_once('.').unwrap_or((v, "0"));
        (a.parse::<u32>().unwrap_or(0), b.parse::<u32>().unwrap_or(0))
    };
    if let Some(rest) = m.strip_prefix("claude-") {
        let kind = rest.split('-').next().unwrap_or_default();
        let (major, minor) = major_minor(version(&format!("claude-{kind}-")));
        return (kind != "haiku" && (major >= 5 || (major == 4 && minor >= 8))).then_some("output_config");
    }
    if m.starts_with("gpt-") {
        let (major, minor) = major_minor(version("gpt-"));
        return (major >= 6 || (major == 5 && minor >= 6)).then_some("reasoning");
    }
    None
}

fn context_window(model_id: &str) -> u64 {
    let m = model_id.to_ascii_lowercase();
    if let Some(rest) = m.strip_prefix("claude-") {
        let v = rest.split('-').nth(1).unwrap_or_default();
        let (a, b) = v.split_once('.').unwrap_or((v, "0"));
        let (major, minor) = (a.parse::<u32>().unwrap_or(0), b.parse::<u32>().unwrap_or(0));
        return if major >= 5 || (major == 4 && minor >= 6) { 1_000_000 } else { 200_000 };
    }
    if m.starts_with("gpt-") { 1_000_000 } else { 200_000 }
}

// -------------------------------------------------------------------- request

fn thinking_length(effort: Option<&str>, budget: Option<u64>) -> u64 {
    match effort {
        Some("max") => 120_000,
        Some("xhigh") => 64_000,
        Some("high") => 32_000,
        Some("medium") => 16_000,
        Some("low") | Some("minimal") => 8_000,
        _ => budget.unwrap_or(32_000).clamp(1024, 200_000),
    }
}

/// Kiro rejects unknown schema keywords and an empty `required` with 400 "Improperly formed request".
fn clean_schema(v: &mut Value) {
    match v {
        Value::Object(map) => {
            map.remove("additionalProperties");
            map.remove("$schema");
            if map.get("required").and_then(Value::as_array).is_some_and(|r| r.is_empty()) {
                map.remove("required");
            }
            for value in map.values_mut() {
                clean_schema(value);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(clean_schema),
        _ => {}
    }
}

fn tool_spec(t: &crate::ir::Tool) -> Value {
    let mut schema = if t.parameters.is_object() { t.parameters.clone() } else { json!({}) };
    clean_schema(&mut schema);
    let obj = schema.as_object_mut().expect("object schema");
    obj.insert("type".into(), "object".into());
    if !obj.get("properties").is_some_and(Value::is_object) {
        obj.insert("properties".into(), json!({}));
    }
    let mut description =
        if t.description.trim().is_empty() { format!("Tool: {}", t.name) } else { t.description.clone() };
    if description.chars().count() > MAX_TOOL_DESCRIPTION {
        description = description.chars().take(MAX_TOOL_DESCRIPTION).collect::<String>() + "...";
    }
    json!({ "toolSpecification": { "name": t.name, "description": description, "inputSchema": { "json": schema } } })
}

#[derive(Default)]
struct UserTurn {
    text: Vec<String>,
    images: Vec<Value>,
    /// (tool use id, tool name if known, text, is_error)
    results: Vec<(String, Option<String>, String, bool)>,
}

#[derive(Default)]
struct AssistantTurn {
    text: String,
    /// (id, name, input)
    calls: Vec<(String, String, Value)>,
}

enum Turn {
    User(UserTurn),
    Assistant(AssistantTurn),
}

fn image_value(mime: &str, data: &str) -> Value {
    let format = match mime.rsplit('/').next().unwrap_or("png") {
        "jpg" => "jpeg",
        other => other,
    };
    json!({ "format": format, "source": { "bytes": data } })
}

fn turns(req: &Request) -> Vec<Turn> {
    let mut out: Vec<Turn> = Vec::new();
    for m in &req.messages {
        match m.role {
            Role::User => {
                let mut t = UserTurn::default();
                for p in &m.parts {
                    match p {
                        Part::Text(s) => t.text.push(s.clone()),
                        Part::Image(Image::Base64 { mime, data }) => t.images.push(image_value(mime, data)),
                        Part::Image(Image::Url(u)) => t.text.push(format!("[image: {u}]")),
                        Part::ToolResult { id, name, content, is_error } => {
                            let mut body = crate::ir::parts_text(content);
                            for c in content {
                                if let Part::Image(Image::Base64 { mime, data }) = c {
                                    t.images.push(image_value(mime, data));
                                    body.push_str("\n[Tool returned an image; the image is attached to this message.]");
                                }
                            }
                            t.results.push((id.clone(), name.clone(), body.trim().to_string(), *is_error));
                        }
                        _ => {}
                    }
                }
                match out.last_mut() {
                    Some(Turn::User(last)) => {
                        last.text.extend(t.text);
                        last.images.extend(t.images);
                        last.results.extend(t.results);
                    }
                    _ => out.push(Turn::User(t)),
                }
            }
            Role::Assistant => {
                let mut t = AssistantTurn::default();
                for p in &m.parts {
                    match p {
                        Part::Text(s) => t.text.push_str(s),
                        Part::ToolCall { id, name, args, .. } => {
                            let input = match crate::ir::parse_args(args) {
                                Value::Object(o) => Value::Object(o),
                                other => json!({ "_raw": other }),
                            };
                            t.calls.push((id.clone(), name.clone(), input));
                        }
                        _ => {}
                    }
                }
                match out.last_mut() {
                    Some(Turn::Assistant(last)) => {
                        last.text.push_str(&t.text);
                        last.calls.extend(t.calls);
                    }
                    _ => out.push(Turn::Assistant(t)),
                }
            }
        }
    }
    out
}

fn narrate(results: &[(String, Option<String>, String, bool)]) -> String {
    let mut s = String::from("Tool results:\n\n");
    for (id, name, text, is_error) in results {
        let label = name.clone().unwrap_or_else(|| id.clone());
        let text = if text.is_empty() { "(no output)" } else { text.as_str() };
        s.push_str(&format!("[{label}]{} {text}\n\n", if *is_error { " (error)" } else { "" }));
    }
    s.trim_end().to_string()
}

fn user_message(model: &str, content: &str, images: &[Value], context: Option<Value>) -> Value {
    let mut m = json!({ "content": content, "modelId": model, "origin": "AI_EDITOR" });
    if !images.is_empty() {
        m["images"] = Value::Array(images.to_vec());
    }
    if let Some(c) = context {
        m["userInputMessageContext"] = c;
    }
    json!({ "userInputMessage": m })
}

fn assistant_message(text: &str, calls: &[(String, String, Value)]) -> Value {
    let mut m = json!({ "content": if text.trim().is_empty() { "(empty)" } else { text } });
    if !calls.is_empty() {
        m["toolUses"] =
            calls.iter().map(|(id, name, input)| json!({ "toolUseId": id, "name": name, "input": input })).collect();
    }
    json!({ "assistantResponseMessage": m })
}

/// One conversation keeps one id, so the data plane can reuse its prompt cache.
fn conversation_id(model: &str, system: &str, first_user: &str) -> String {
    let digest = Sha256::digest(format!("{model}\n{system}\n{first_user}").as_bytes());
    let mut b: [u8; 16] = digest[..16].try_into().expect("16 bytes");
    b[6] = (b[6] & 0x0f) | 0x50;
    b[8] = (b[8] & 0x3f) | 0x80;
    uuid::Uuid::from_bytes(b).to_string()
}

/// IR -> Kiro's conversation state; `prepare` adds the profile and the address.
pub fn build_request(req: &Request, model: &str) -> Value {
    let id = model_id(model);
    let (id, thinking_suffix) = match id.strip_suffix("-thinking") {
        Some(b) => (b.to_string(), true),
        None => (id, false),
    };
    let reasoning = req.reasoning.as_ref().filter(|r| !r.disabled);
    let effort = reasoning.and_then(|r| r.effort_level()).filter(|e| e != "none").map(|e| match e.as_str() {
        "minimal" => "low".to_string(),
        _ => e,
    });
    let thinking = thinking_suffix || reasoning.is_some_and(|r| r.budget.is_some() || r.effort.is_some());

    let mut system = req.system.join("\n\n").trim().to_string();
    if thinking {
        let length = thinking_length(effort.as_deref(), reasoning.and_then(|r| r.budget));
        let prefix =
            format!("<thinking_mode>enabled</thinking_mode>\n<max_thinking_length>{length}</max_thinking_length>");
        system = if system.is_empty() { prefix } else { format!("{prefix}\n\n{system}") };
    }

    let mut turns = turns(req);
    // The conversation starts with the user, and the current message is the user's.
    while matches!(turns.first(), Some(Turn::Assistant(_))) {
        turns.remove(0);
    }
    if !matches!(turns.last(), Some(Turn::User(_))) {
        turns.push(Turn::User(UserTurn { text: vec![".".into()], ..Default::default() }));
    }
    let Some(Turn::User(current)) = turns.pop() else { unreachable!("a user turn was pushed") };

    // Only the latest tool calls stay structured, and only when the current message
    // answers every one of them; everything older is told as text.
    let structured = match turns.last() {
        Some(Turn::Assistant(a)) if !a.calls.is_empty() => {
            a.calls.iter().all(|(id, _, _)| current.results.iter().any(|(rid, ..)| rid == id))
        }
        _ => false,
    };
    let mut history: Vec<Value> = Vec::new();
    if !system.is_empty() {
        history.push(user_message(&id, &system, &[], None));
        history.push(assistant_message(PRIMING_REPLY, &[]));
    }
    let first_user = turns
        .iter()
        .find_map(|t| match t {
            Turn::User(u) => Some(u.text.join("\n")),
            _ => None,
        })
        .unwrap_or_else(|| current.text.join("\n"));
    let last = turns.len().checked_sub(1);
    for (i, t) in turns.iter().enumerate() {
        match t {
            Turn::User(u) => {
                let mut content = u.text.join("\n");
                if !u.results.is_empty() {
                    if !content.is_empty() {
                        content.push_str("\n\n");
                    }
                    content.push_str(&narrate(&u.results));
                }
                if content.trim().is_empty() {
                    content =
                        if u.images.is_empty() { ".".into() } else { "Please analyze the attached image.".into() };
                }
                history.push(user_message(&id, &content, &u.images, None));
            }
            Turn::Assistant(a) => {
                let calls: &[(String, String, Value)] = if structured && Some(i) == last { &a.calls } else { &[] };
                history.push(assistant_message(&a.text, calls));
            }
        }
    }

    let mut content = current.text.join("\n");
    let mut context = Map::new();
    if structured {
        context.insert(
            "toolResults".into(),
            current
                .results
                .iter()
                .map(|(rid, _, text, is_error)| {
                    let text = if text.is_empty() { "(no output)" } else { text.as_str() };
                    json!({ "toolUseId": rid, "status": if *is_error { "error" } else { "success" }, "content": [{ "text": text }] })
                })
                .collect(),
        );
    } else if !current.results.is_empty() {
        if !content.is_empty() {
            content.push_str("\n\n");
        }
        content.push_str(&narrate(&current.results));
    }
    if content.trim().is_empty() {
        content = if current.images.is_empty() { ".".into() } else { "Please analyze the attached image.".into() };
    }
    if !req.tools.is_empty() {
        context.insert("tools".into(), req.tools.iter().map(tool_spec).collect());
    }
    let current_message =
        user_message(&id, &content, &current.images, (!context.is_empty()).then(|| Value::Object(context)));

    let mut inference = Map::new();
    inference.insert("maxTokens".into(), req.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS).max(1).into());
    if !thinking {
        if let Some(t) = req.temperature {
            inference.insert("temperature".into(), t.into());
        }
        if let Some(p) = req.top_p {
            inference.insert("topP".into(), p.into());
        }
    }
    let mut body = json!({
        "conversationState": {
            "agentTaskType": "vibe",
            "chatTriggerType": "MANUAL",
            "conversationId": conversation_id(&id, &system, &first_user),
            "currentMessage": current_message,
            "history": history,
        },
        "inferenceConfig": inference,
    });
    if let (Some(level), Some(path)) = (&effort, supports_effort(&id)) {
        body["additionalModelRequestFields"] = json!({ path: { "effort": level } });
    }
    body
}

pub fn prepare(t: &Target, mut body: Value) -> Prepared {
    let c = match &*t.acct.cred.read() {
        Credential::OAuth(o) => creds(o, &t.acct.device_id),
        Credential::ApiKey { key, .. } => Creds {
            token: key.clone(),
            profile_arn: None,
            region: DEFAULT_REGION.into(),
            machine_id: t.acct.device_id.clone(),
        },
    };
    if let Some(arn) = &c.profile_arn {
        body["profileArn"] = arn.clone().into();
    }
    let headers = vec![
        ("authorization".into(), format!("Bearer {}", c.token)),
        ("content-type".into(), "application/json".into()),
        ("accept".into(), "*/*".into()),
        ("user-agent".into(), user_agent("codewhispererstreaming", STREAM_SDK, "E", &c.machine_id)),
        ("x-amz-user-agent".into(), short_agent(STREAM_SDK, &c.machine_id)),
        ("x-amzn-kiro-agent-mode".into(), "vibe".into()),
        ("x-amzn-codewhisperer-optout".into(), "true".into()),
        ("amz-sdk-invocation-id".into(), uuid::Uuid::new_v4().to_string()),
        ("amz-sdk-request".into(), "attempt=1; max=2".into()),
    ];
    Prepared { url: format!("{}/generateAssistantResponse", data_plane(&c.region)), headers, body, raw: None }
}

// --------------------------------------------------------------------- stream

/// One decoded AWS event-stream message.
struct Frame {
    headers: Vec<(String, String)>,
    payload: Vec<u8>,
}

impl Frame {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
    }
}

/// Splits one frame off `buf`: `None` until a whole frame is there, an error for a
/// corrupt one (the stream cannot be resynchronised after that).
fn next_frame(buf: &mut BytesMut) -> Result<Option<Frame>> {
    if buf.len() < 12 {
        return Ok(None);
    }
    let total = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    let headers_len = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]) as usize;
    let prelude_crc = u32::from_be_bytes([buf[8], buf[9], buf[10], buf[11]]);
    if !(16..=MAX_FRAME).contains(&total) || headers_len > total - 16 {
        bail!("corrupt event-stream prelude (length {total}, headers {headers_len})");
    }
    if crc32fast::hash(&buf[..8]) != prelude_crc {
        bail!("event-stream prelude checksum mismatch");
    }
    if buf.len() < total {
        return Ok(None);
    }
    let frame = buf.split_to(total);
    let body = &frame[..total - 4];
    let message_crc = u32::from_be_bytes([frame[total - 4], frame[total - 3], frame[total - 2], frame[total - 1]]);
    if crc32fast::hash(body) != message_crc {
        bail!("event-stream message checksum mismatch");
    }
    let mut headers = Vec::new();
    let mut h = &body[12..12 + headers_len];
    while !h.is_empty() {
        let name_len = h[0] as usize;
        let name = String::from_utf8_lossy(h.get(1..1 + name_len).unwrap_or_default()).into_owned();
        h = h.get(1 + name_len..).unwrap_or_default();
        let Some((&kind, rest)) = h.split_first() else { break };
        h = rest;
        let value_len = match kind {
            0 | 1 => 0,
            2 => 1,
            3 => 2,
            4 => 4,
            5 | 8 => 8,
            9 => 16,
            6 | 7 => {
                let Some(len) = h.get(..2) else { break };
                h = &h[2..];
                u16::from_be_bytes([len[0], len[1]]) as usize
            }
            _ => break,
        };
        let Some(value) = h.get(..value_len) else { break };
        if kind == 7 {
            headers.push((name, String::from_utf8_lossy(value).into_owned()));
        }
        h = &h[value_len..];
    }
    Ok(Some(Frame { headers, payload: body[12 + headers_len..].to_vec() }))
}

/// `:message-type: exception` frames name the fault in a header and the detail in the payload.
fn exception(frame: &Frame) -> (u16, String) {
    let kind = frame.header(":exception-type").or_else(|| frame.header(":error-code")).unwrap_or_default();
    let detail: Value = serde_json::from_slice(&frame.payload).unwrap_or_default();
    let message = detail["message"]
        .as_str()
        .or_else(|| detail["Message"].as_str())
        .or_else(|| frame.header(":error-message"))
        .filter(|m| !m.is_empty())
        .map(String::from)
        .unwrap_or_else(|| if kind.is_empty() { "Kiro stream error".into() } else { kind.to_string() });
    let lower = kind.to_ascii_lowercase();
    let status = match () {
        _ if lower.contains("throttl") || lower.contains("quota") => 429,
        _ if lower.contains("accessdenied") || lower.contains("forbidden") => 403,
        _ if lower.contains("validation") => 400,
        _ if lower.contains("unauthorized") || lower.contains("expiredtoken") => 401,
        _ if lower.contains("serviceunavailable") => 503,
        _ => 502,
    };
    (status, if kind.is_empty() { message } else { format!("{kind}: {message}") })
}

const THINK_OPEN: &str = "<thinking>";
const THINK_CLOSE: &str = "</thinking>";

/// Thinking arrives either as `reasoningContentEvent` or, with the prompt-level
/// thinking mode, as `<thinking>` tags inside the text; the tags are split out, even
/// across frames.
#[derive(Default)]
struct TextSplitter {
    pending: String,
    thinking: bool,
}

impl TextSplitter {
    fn held_tail(s: &str, tag: &str) -> usize {
        (1..tag.len()).rev().find(|&k| s.ends_with(&tag[..k])).unwrap_or(0)
    }

    fn push(&mut self, delta: &str, out: &mut Vec<Event>) {
        self.pending.push_str(delta);
        loop {
            let (tag, next) = if self.thinking { (THINK_CLOSE, false) } else { (THINK_OPEN, true) };
            if let Some(at) = self.pending.find(tag) {
                let before = self.pending[..at].to_string();
                self.emit(&before, out);
                self.pending = self.pending[at + tag.len()..].to_string();
                self.thinking = next;
                continue;
            }
            let keep = Self::held_tail(&self.pending, tag);
            let ready = self.pending[..self.pending.len() - keep].to_string();
            self.emit(&ready, out);
            self.pending = self.pending[self.pending.len() - keep..].to_string();
            return;
        }
    }

    fn emit(&self, text: &str, out: &mut Vec<Event>) {
        if text.is_empty() {
            return;
        }
        if self.thinking {
            out.push(Event::Reasoning(text.to_string()));
        } else {
            out.push(Event::Text(text.to_string()));
        }
    }

    fn finish(&mut self, out: &mut Vec<Event>) {
        let rest = std::mem::take(&mut self.pending);
        self.emit(&rest, out);
    }
}

struct Decoder {
    model: String,
    names: HashMap<String, String>,
    started: bool,
    text: TextSplitter,
    /// Tool use ids in arrival order; the key is the position.
    tools: Vec<String>,
    /// Whole-object inputs, replaced as they grow, sent once the tool stops.
    object_inputs: HashMap<usize, String>,
    fragments_seen: Vec<bool>,
    stopped: Vec<bool>,
    current: Option<usize>,
    output_chars: u64,
    content: bool,
    stop_reason: Option<String>,
    context_pct: Option<f64>,
    credits: f64,
}

impl Decoder {
    fn new(model: String, names: HashMap<String, String>) -> Decoder {
        Decoder {
            model,
            names,
            started: false,
            text: TextSplitter::default(),
            tools: Vec::new(),
            object_inputs: HashMap::new(),
            fragments_seen: Vec::new(),
            stopped: Vec::new(),
            current: None,
            output_chars: 0,
            content: false,
            stop_reason: None,
            context_pct: None,
            credits: 0.0,
        }
    }

    fn start(&mut self, out: &mut Vec<Event>) {
        if !self.started {
            self.started = true;
            out.push(Event::Start { id: None, model: Some(self.model.clone()) });
        }
    }

    fn frame(&mut self, frame: &Frame, out: &mut Vec<Event>) -> Result<()> {
        let message_type = frame.header(":message-type").unwrap_or("event");
        if message_type == "exception" || message_type == "error" {
            let (status, message) = exception(frame);
            out.push(Event::Error { status, message });
            return Ok(());
        }
        let event = frame.header(":event-type").unwrap_or_default().to_string();
        let v: Value = if frame.payload.iter().all(u8::is_ascii_whitespace) {
            Value::Null
        } else {
            serde_json::from_slice(&frame.payload).unwrap_or(Value::Null)
        };
        match event.as_str() {
            "assistantResponseEvent" | "codeEvent" => {
                if let Some(t) = v["content"].as_str().filter(|t| !t.is_empty()) {
                    self.start(out);
                    self.content = true;
                    self.output_chars += t.chars().count() as u64;
                    self.text.push(t, out);
                }
            }
            "reasoningContentEvent" => {
                let t = v["text"].as_str().or_else(|| v["reasoningText"]["text"].as_str()).unwrap_or_default();
                if !t.is_empty() {
                    self.start(out);
                    self.content = true;
                    self.output_chars += t.chars().count() as u64;
                    out.push(Event::Reasoning(t.to_string()));
                }
            }
            "toolUseEvent" => {
                self.start(out);
                self.content = true;
                match &v {
                    Value::Array(items) => {
                        for item in items {
                            self.tool_use(item, out);
                        }
                    }
                    other => self.tool_use(other, out),
                }
            }
            "metadataEvent" | "messageMetadataEvent" => {
                if let Some(r) = v["stopReason"].as_str().or_else(|| v["stop_reason"].as_str()) {
                    self.stop_reason = Some(r.to_string());
                }
            }
            "contextUsageEvent" => self.context_pct = v["contextUsagePercentage"].as_f64(),
            "meteringEvent" => {
                let m = if v["meteringEvent"].is_object() { &v["meteringEvent"] } else { &v };
                if let Some(u) = m["usage"].as_f64() {
                    self.credits += u;
                }
            }
            "invalidStateEvent" => {
                let message = v["message"].as_str().unwrap_or("Kiro reported an invalid conversation state");
                out.push(Event::Error { status: 400, message: message.to_string() });
            }
            _ => {}
        }
        Ok(())
    }

    fn tool_use(&mut self, v: &Value, out: &mut Vec<Event>) {
        let s = |keys: &[&str]| keys.iter().find_map(|k| v[*k].as_str()).map(str::trim).filter(|s| !s.is_empty());
        let id = s(&["toolUseId", "toolUseID", "tool_use_id", "id"]).map(String::from);
        let name = s(&["name", "toolName", "tool_name"]).map(String::from);
        let key = match &id {
            Some(id) => self.tools.iter().position(|t| t == id),
            None => self.current.filter(|&k| !self.stopped[k]),
        };
        let key = match key {
            Some(k) => k,
            None => {
                let Some(name) = name.clone() else { return };
                let key = self.tools.len();
                let id = id.clone().unwrap_or_else(|| crate::ir::new_id("toolu_"));
                self.tools.push(id.clone());
                self.fragments_seen.push(false);
                self.stopped.push(false);
                let name = self.names.get(&name).cloned().unwrap_or(name);
                out.push(Event::ToolStart { key, id, name });
                key
            }
        };
        self.current = Some(key);
        match &v["input"] {
            Value::String(fragment) if !fragment.is_empty() => {
                self.fragments_seen[key] = true;
                self.output_chars += fragment.chars().count() as u64;
                out.push(Event::ToolArgs { key, delta: fragment.clone() });
            }
            Value::Object(_) | Value::Array(_) => {
                self.object_inputs.insert(key, v["input"].to_string());
            }
            _ => {}
        }
        if ["stop", "isStop", "done"].iter().any(|k| v[*k] == true) {
            self.finish_tool(key, out);
        }
    }

    fn finish_tool(&mut self, key: usize, out: &mut Vec<Event>) {
        if self.stopped[key] {
            return;
        }
        self.stopped[key] = true;
        if !self.fragments_seen[key]
            && let Some(input) = self.object_inputs.remove(&key)
        {
            self.output_chars += input.chars().count() as u64;
            out.push(Event::ToolArgs { key, delta: input });
        }
    }

    fn end(&mut self, out: &mut Vec<Event>) {
        self.text.finish(out);
        for key in 0..self.tools.len() {
            self.finish_tool(key, out);
        }
        if !self.content {
            out.push(Event::Error { status: 502, message: "Kiro stream ended without any content".into() });
            return;
        }
        let input = self.context_pct.map(|p| (p.max(0.0) / 100.0 * context_window(&self.model) as f64) as u64);
        if input.is_some() || self.output_chars > 0 {
            out.push(Event::Usage(Usage {
                input: input.unwrap_or(0),
                output: self.output_chars.div_ceil(4),
                ..Default::default()
            }));
        }
        if self.credits > 0.0 {
            tracing::debug!(model = %self.model, credits = self.credits, "kiro metering");
        }
        let reason = self.stop_reason.as_deref().unwrap_or_default().to_ascii_lowercase();
        out.push(Event::Finish(match reason.as_str() {
            "max_tokens" | "length" => Finish::Length,
            "content_filtered" | "guardrail_intervened" => Finish::Filter,
            _ if !self.tools.is_empty() => Finish::ToolCalls,
            _ => Finish::Stop,
        }));
    }
}

pub fn event_stream(
    resp: reqwest::Response,
    names: HashMap<String, String>,
    model: String,
) -> Pin<Box<dyn Stream<Item = Event> + Send>> {
    Box::pin(async_stream::stream! {
        let mut body = resp.bytes_stream();
        let mut buf = BytesMut::new();
        let mut dec = Decoder::new(model, names);
        let mut out = Vec::new();
        let mut failed = false;
        'read: while let Some(chunk) = body.next().await {
            match chunk {
                Ok(c) => buf.extend_from_slice(&c),
                Err(e) => {
                    out.push(Event::Error { status: 502, message: format!("upstream stream error: {e}") });
                    failed = true;
                    break;
                }
            }
            loop {
                match next_frame(&mut buf) {
                    Ok(Some(frame)) => {
                        let _ = dec.frame(&frame, &mut out);
                        if out.iter().any(|e| matches!(e, Event::Error { .. })) {
                            failed = true;
                            for ev in out.drain(..) { yield ev; }
                            break 'read;
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        out.push(Event::Error { status: 502, message: format!("{e:#}") });
                        failed = true;
                        for ev in out.drain(..) { yield ev; }
                        break 'read;
                    }
                }
            }
            for ev in out.drain(..) { yield ev; }
        }
        if !failed {
            buf.advance(buf.len());
            dec.end(&mut out);
        }
        for ev in out.drain(..) { yield ev; }
    })
}

// ----------------------------------------------------------------------- login

/// A sign-in in progress: the registered OIDC client and the PKCE verifier.
#[derive(Debug, Clone)]
pub struct Pending {
    pub region: String,
    pub start_url: String,
    pub client_id: String,
    pub client_secret: String,
    pub redirect: String,
    pub verifier: String,
    /// "idc" or "builder-id".
    pub method: &'static str,
}

#[derive(Debug, Clone)]
pub struct DeviceStart {
    pub pending: Pending,
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub interval: u64,
    pub expires_in: u64,
}

async fn oidc_post(http: &reqwest::Client, url: &str, body: &Value) -> Result<Value> {
    let resp = http
        .post(url)
        .header("content-type", "application/json")
        .header("accept", "application/json")
        .json(body)
        .send()
        .await?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("{url} returned {status}: {}", text.chars().take(300).collect::<String>());
    }
    serde_json::from_str(&text).with_context(|| format!("invalid response from {url}"))
}

async fn register(
    http: &reqwest::Client,
    region: &str,
    start_url: &str,
    grant_types: &[&str],
    redirect: Option<&str>,
) -> Result<(String, String)> {
    let mut body = json!({
        "clientName": "Kiro",
        "clientType": "public",
        "scopes": SCOPES,
        "grantTypes": grant_types,
        "issuerUrl": start_url,
    });
    if let Some(r) = redirect {
        body["redirectUris"] = json!([r]);
    }
    let v = oidc_post(http, &format!("{}/client/register", oidc(region)), &body).await?;
    let id = v["clientId"].as_str().ok_or_else(|| anyhow!("no clientId in the registration"))?;
    let secret = v["clientSecret"].as_str().ok_or_else(|| anyhow!("no clientSecret in the registration"))?;
    Ok((id.to_string(), secret.to_string()))
}

/// Identity Center sign-in: registers a client for `start_url` and returns the browser URL.
pub async fn begin_pkce(
    http: &reqwest::Client,
    start_url: &str,
    region: &str,
    redirect: &str,
    state: &str,
    pkce: &crate::oauth::Pkce,
) -> Result<(String, Pending)> {
    if !valid_region(region) {
        bail!("`{region}` is not an AWS region (expected something like us-east-1)");
    }
    let start_url = start_url.trim().trim_end_matches('/').to_string();
    if !start_url.starts_with("https://") {
        bail!("the start URL must begin with https://");
    }
    let (client_id, client_secret) =
        register(http, region, &start_url, &["authorization_code", "refresh_token"], Some(redirect))
            .await
            .map_err(|e| {
                // AWS says only "invalid_request" when the start URL belongs to another region.
                if format!("{e:#}").contains("invalid_request") {
                    anyhow!("AWS refused the registration in {region}: check that {region} is the region of this Identity Center (its AWS access portal shows it)")
                } else {
                    e
                }
            })?;
    let q = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs([
            ("response_type", "code"),
            ("client_id", client_id.as_str()),
            ("redirect_uri", redirect),
            ("scopes", &SCOPES.join(",")),
            ("state", state),
            ("code_challenge", pkce.challenge.as_str()),
            ("code_challenge_method", "S256"),
        ])
        .finish();
    let url = format!("{}/authorize?{q}", oidc(region));
    let pending = Pending {
        region: region.to_string(),
        start_url,
        client_id,
        client_secret,
        redirect: redirect.to_string(),
        verifier: pkce.verifier.clone(),
        method: "idc",
    };
    Ok((url, pending))
}

/// Builder ID sign-in: a device code to approve at AWS.
pub async fn begin_device(http: &reqwest::Client, region: &str) -> Result<DeviceStart> {
    if !valid_region(region) {
        bail!("`{region}` is not an AWS region (expected something like us-east-1)");
    }
    let (client_id, client_secret) = register(
        http,
        region,
        BUILDER_ID_START,
        &["urn:ietf:params:oauth:grant-type:device_code", "refresh_token"],
        None,
    )
    .await?;
    let v = oidc_post(
        http,
        &format!("{}/device_authorization", oidc(region)),
        &json!({ "clientId": client_id, "clientSecret": client_secret, "startUrl": BUILDER_ID_START }),
    )
    .await?;
    let s = |k: &str| v[k].as_str().map(String::from);
    Ok(DeviceStart {
        pending: Pending {
            region: region.to_string(),
            start_url: BUILDER_ID_START.into(),
            client_id,
            client_secret,
            redirect: String::new(),
            verifier: String::new(),
            method: "builder-id",
        },
        device_code: s("deviceCode").ok_or_else(|| anyhow!("no device code"))?,
        user_code: s("userCode").ok_or_else(|| anyhow!("no user code"))?,
        verification_uri: s("verificationUriComplete").or_else(|| s("verificationUri")).unwrap_or_default(),
        interval: v["interval"].as_u64().unwrap_or(5).max(1),
        expires_in: v["expiresIn"].as_u64().unwrap_or(600),
    })
}

/// Polls until the user approves the device code.
pub async fn wait_device(http: &reqwest::Client, d: &DeviceStart) -> Result<Signed> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(d.expires_in.min(1800));
    let mut interval = d.interval;
    let url = format!("{}/token", oidc(&d.pending.region));
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(interval)).await;
        if std::time::Instant::now() > deadline {
            bail!("the code expired before it was approved");
        }
        let resp = http
            .post(&url)
            .header("content-type", "application/json")
            .json(&json!({
                "clientId": d.pending.client_id,
                "clientSecret": d.pending.client_secret,
                "grantType": "urn:ietf:params:oauth:grant-type:device_code",
                "deviceCode": d.device_code,
            }))
            .send()
            .await?;
        let status = resp.status();
        let v: Value = resp.json().await.unwrap_or_default();
        if status.is_success() {
            return finish(http, &d.pending, &v).await;
        }
        match v["error"].as_str().unwrap_or_default() {
            "authorization_pending" => {}
            "slow_down" => interval += 5,
            "access_denied" => bail!("sign-in was denied"),
            "expired_token" => bail!("the code expired before it was approved"),
            other => bail!("AWS returned {status}: {}", if other.is_empty() { v.to_string() } else { other.into() }),
        }
    }
}

/// Exchanges the authorization code the browser came back with.
pub async fn complete_pkce(http: &reqwest::Client, p: &Pending, code: &str) -> Result<Signed> {
    let v = oidc_post(
        http,
        &format!("{}/token", oidc(&p.region)),
        &json!({
            "clientId": p.client_id,
            "clientSecret": p.client_secret,
            "grantType": "authorization_code",
            "redirectUri": p.redirect,
            "code": code,
            "codeVerifier": p.verifier,
        }),
    )
    .await?;
    finish(http, p, &v).await
}

fn token_expiry(v: &Value) -> Option<DateTime<Utc>> {
    v["expiresIn"].as_i64().map(|s| Utc::now() + chrono::Duration::seconds(s))
}

/// Builds the credential from a token response: finds the profile (Identity Center
/// accounts need one), the subscription and the email.
async fn finish(http: &reqwest::Client, p: &Pending, v: &Value) -> Result<Signed> {
    let access = v["accessToken"].as_str().ok_or_else(|| anyhow!("no accessToken in the response"))?.to_string();
    let refresh = v["refreshToken"].as_str().unwrap_or_default().to_string();
    let machine_id = uuid::Uuid::new_v4().to_string();
    let profile = match resolve_profile(http, &access, &p.region, &machine_id).await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("kiro profile lookup failed: {e:#}");
            None
        }
    };
    let region = profile.as_ref().and_then(|(arn, _)| arn_region(arn)).unwrap_or(DEFAULT_REGION).to_string();
    let usage = fetch_usage(http, &access, profile.as_ref().map(|(a, _)| a.as_str()), &region, &machine_id)
        .await
        .unwrap_or_default();
    let email = usage["userInfo"]["email"]
        .as_str()
        .filter(|e| !e.is_empty())
        .map(String::from)
        .or_else(|| profile.as_ref().and_then(|(_, name)| name.clone()))
        .unwrap_or_else(|| format!("user-{}", &hex::encode(Sha256::digest(access.as_bytes()))[..12]));
    let oauth = OAuth {
        access_token: access,
        refresh_token: refresh,
        expires_at: token_expiry(v),
        email: Some(email.clone()),
        ..Default::default()
    };
    let mut extra: Vec<(&'static str, Value)> = vec![
        ("auth_method", p.method.into()),
        ("region", p.region.clone().into()),
        ("start_url", p.start_url.clone().into()),
        ("client_id", p.client_id.clone().into()),
        ("client_secret", p.client_secret.clone().into()),
        ("machine_id", machine_id.into()),
        ("profile_region", region.into()),
    ];
    if let Some((arn, _)) = &profile {
        extra.push(("profile_arn", arn.clone().into()));
    }
    Ok(Signed { oauth, file: format!("kiro-{}.json", crate::oauth::file_safe(&email)), extra })
}

/// Refreshes an access token. AWS rotates the refresh token, so the new one is kept.
pub async fn refresh(http: &reqwest::Client, old: &OAuth) -> Result<(OAuth, Vec<(&'static str, Value)>)> {
    if old.refresh_token.is_empty() {
        bail!("no refresh token; sign in again");
    }
    let v = if old.field("auth_method") == Some("social") {
        oidc_post(http, SOCIAL_REFRESH, &json!({ "refreshToken": old.refresh_token })).await?
    } else {
        let (Some(client_id), Some(client_secret)) = (old.field("client_id"), old.field("client_secret")) else {
            bail!("the credential has no OIDC client; sign in again");
        };
        let region = old.field("region").filter(|r| valid_region(r)).unwrap_or(DEFAULT_REGION);
        oidc_post(
            http,
            &format!("{}/token", oidc(region)),
            &json!({
                "clientId": client_id,
                "clientSecret": client_secret,
                "refreshToken": old.refresh_token,
                "grantType": "refresh_token",
            }),
        )
        .await?
    };
    let access = v["accessToken"].as_str().ok_or_else(|| anyhow!("no accessToken in the refresh"))?.to_string();
    let new = OAuth {
        access_token: access,
        refresh_token: v["refreshToken"].as_str().filter(|r| !r.is_empty()).unwrap_or(&old.refresh_token).into(),
        expires_at: token_expiry(&v).or(old.expires_at),
        ..old.clone()
    };
    let mut extra = Vec::new();
    if let Some(arn) = v["profileArn"].as_str().filter(|a| !a.is_empty())
        && old.field("profile_arn").is_none()
    {
        extra.push(("profile_arn", arn.into()));
    }
    Ok((new, extra))
}

// --------------------------------------------------------------- data plane

async fn rest(
    http: &reqwest::Client,
    rb: reqwest::RequestBuilder,
    token: &str,
    machine: &str,
) -> Result<(reqwest::StatusCode, Value)> {
    let _ = http;
    let resp =
        with_headers(rb, &runtime_headers(token, machine)).timeout(std::time::Duration::from_secs(20)).send().await?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    Ok((status, serde_json::from_str(&text).unwrap_or_else(|_| json!({ "message": text }))))
}

/// The profile an Identity Center token works with: `(arn, profile name)`. Several
/// profiles are told apart by their subscription, as the default one may have none.
/// Builder ID has no profiles.
pub async fn resolve_profile(
    http: &reqwest::Client,
    token: &str,
    region_hint: &str,
    machine: &str,
) -> Result<Option<(String, Option<String>)>> {
    let mut regions = vec![DEFAULT_REGION.to_string(), "eu-central-1".into()];
    if valid_region(region_hint) && !regions.iter().any(|r| r == region_hint) {
        regions.push(region_hint.to_string());
    }
    let mut profiles: Vec<(String, Option<String>)> = Vec::new();
    let mut last_error = None;
    for region in &regions {
        let url = format!("{}/ListAvailableProfiles", data_plane(region));
        let mut next: Option<String> = None;
        loop {
            let body = match &next {
                Some(t) => json!({ "nextToken": t }),
                None => json!({}),
            };
            let rb = http.post(&url).header("content-type", "application/json").json(&body);
            let (status, v) = rest(http, rb, token, machine).await?;
            if status == 403 && v["message"].as_str().unwrap_or_default().contains("Builder ID") {
                return Ok(None);
            }
            if !status.is_success() {
                last_error = Some(format!("{url} returned {status}: {}", v["message"].as_str().unwrap_or_default()));
                break;
            }
            for p in v["profiles"].as_array().into_iter().flatten() {
                if let Some(arn) = p["arn"].as_str().or_else(|| p["profileArn"].as_str()) {
                    profiles.push((arn.to_string(), p["profileName"].as_str().map(String::from)));
                }
            }
            next = v["nextToken"].as_str().filter(|t| !t.is_empty()).map(String::from);
            if next.is_none() || profiles.len() > 100 {
                break;
            }
        }
        if !profiles.is_empty() {
            break;
        }
    }
    if profiles.is_empty() {
        return match last_error {
            Some(e) => Err(anyhow!(e)),
            None => Ok(None),
        };
    }
    if profiles.len() == 1 {
        return Ok(profiles.pop());
    }
    for (arn, name) in &profiles {
        let region = arn_region(arn).unwrap_or(DEFAULT_REGION);
        if let Ok(v) = fetch_usage(http, token, Some(arn), region, machine).await
            && v["subscriptionInfo"]["subscriptionTitle"].as_str().is_some_and(|t| !t.is_empty())
        {
            return Ok(Some((arn.clone(), name.clone())));
        }
    }
    Ok(profiles.into_iter().next())
}

/// `getUsageLimits`: credits used of the month's allowance, and the plan.
pub async fn fetch_usage(
    http: &reqwest::Client,
    token: &str,
    profile_arn: Option<&str>,
    region: &str,
    machine: &str,
) -> Result<Value> {
    let mut query = vec![
        ("origin", "AI_EDITOR".to_string()),
        ("resourceType", "AGENTIC_REQUEST".to_string()),
        ("isEmailRequired", "true".to_string()),
    ];
    if let Some(arn) = profile_arn {
        query.push(("profileArn", arn.to_string()));
    }
    let rb = http.get(format!("{}/getUsageLimits", data_plane(region))).query(&query);
    let (status, v) = rest(http, rb, token, machine).await?;
    if !status.is_success() {
        bail!("getUsageLimits returned {status}: {}", v["message"].as_str().unwrap_or_default());
    }
    Ok(v)
}

/// A usage response as quota windows plus the plan name (`KIRO POWER`).
pub fn windows(v: &Value) -> (Vec<Window>, Option<String>) {
    let plan = v["subscriptionInfo"]["subscriptionTitle"]
        .as_str()
        .or_else(|| v["subscriptionInfo"]["subscriptionType"].as_str())
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(String::from);
    let resets_at = v["nextDateReset"]
        .as_f64()
        .map(|s| s as i64)
        .or_else(|| v["nextDateReset"].as_str().and_then(|s| s.parse().ok()))
        .filter(|s| *s > 0)
        .and_then(|s| DateTime::from_timestamp(s, 0));
    let number = |b: &Value, precise: &str, plain: &str| b[precise].as_f64().or_else(|| b[plain].as_f64());
    let windows = v["usageBreakdownList"]
        .as_array()
        .into_iter()
        .flatten()
        // One breakdown per subscription; its `resourceType` is `CREDIT` on current plans.
        .take(1)
        .filter_map(|b| {
            let limit = number(b, "usageLimitWithPrecision", "usageLimit").filter(|l| *l > 0.0)?;
            let used = number(b, "currentUsageWithPrecision", "currentUsage").unwrap_or(0.0);
            Some(Window { name: WINDOW.into(), used: (used / limit * 100.0).clamp(0.0, 100.0), resets_at, model: None })
        })
        .collect();
    (windows, plan)
}

/// `ListAvailableModels`, as Fusebox names them.
pub async fn fetch_models(
    http: &reqwest::Client,
    token: &str,
    profile_arn: Option<&str>,
    region: &str,
    machine: &str,
) -> Result<Vec<String>> {
    let mut query = vec![("origin", "AI_EDITOR".to_string())];
    if let Some(arn) = profile_arn {
        query.push(("profileArn", arn.to_string()));
    }
    let rb = http.get(format!("{}/ListAvailableModels", data_plane(region))).query(&query);
    let (status, v) = rest(http, rb, token, machine).await?;
    if !status.is_success() {
        bail!("ListAvailableModels returned {status}: {}", v["message"].as_str().unwrap_or_default());
    }
    let list = v["models"].as_array().or_else(|| v["availableModels"].as_array());
    let mut out: Vec<String> = Vec::new();
    for m in list.into_iter().flatten() {
        if let Some(id) = m["modelId"].as_str().filter(|id| !id.is_empty()) {
            let name = public_name(id);
            if !out.iter().any(|o| o.eq_ignore_ascii_case(&name)) {
                out.push(name);
            }
        }
    }
    Ok(out)
}

/// What an account's requests use: `(token, profile, data-plane region, machine id)`.
pub fn account_creds(acct: &crate::accounts::Account) -> Option<(String, Option<String>, String, String)> {
    match &*acct.cred.read() {
        Credential::OAuth(o) if o.base_url.is_none() => {
            let c = creds(o, &acct.device_id);
            Some((c.token, c.profile_arn, c.region, c.machine_id))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::Message;

    fn frame(headers: &[(&str, &str)], payload: &[u8]) -> Vec<u8> {
        let mut h = Vec::new();
        for (name, value) in headers {
            h.push(name.len() as u8);
            h.extend_from_slice(name.as_bytes());
            h.push(7);
            h.extend_from_slice(&(value.len() as u16).to_be_bytes());
            h.extend_from_slice(value.as_bytes());
        }
        let total = 12 + h.len() + payload.len() + 4;
        let mut out = Vec::with_capacity(total);
        out.extend_from_slice(&(total as u32).to_be_bytes());
        out.extend_from_slice(&(h.len() as u32).to_be_bytes());
        let prelude_crc = crc32fast::hash(&out);
        out.extend_from_slice(&prelude_crc.to_be_bytes());
        out.extend_from_slice(&h);
        out.extend_from_slice(payload);
        let crc = crc32fast::hash(&out);
        out.extend_from_slice(&crc.to_be_bytes());
        out
    }

    fn event(kind: &str, payload: &str) -> Vec<u8> {
        frame(
            &[(":event-type", kind), (":content-type", "application/json"), (":message-type", "event")],
            payload.as_bytes(),
        )
    }

    fn decode(bytes: &[u8], model: &str) -> Vec<Event> {
        let mut buf = BytesMut::from(bytes);
        let mut dec = Decoder::new(model.into(), HashMap::new());
        let mut out = Vec::new();
        while let Some(f) = next_frame(&mut buf).unwrap() {
            dec.frame(&f, &mut out).unwrap();
        }
        dec.end(&mut out);
        out
    }

    #[test]
    fn model_ids_use_dots_like_kiro() {
        assert_eq!(model_id("claude-opus-4-8"), "claude-opus-4.8");
        assert_eq!(model_id("claude-sonnet-5"), "claude-sonnet-5");
        assert_eq!(model_id("claude-haiku-4-5-20251001"), "claude-haiku-4.5");
        assert_eq!(model_id("claude-opus-4-8-thinking"), "claude-opus-4.8-thinking");
        assert_eq!(model_id("gpt-5-6-sol"), "gpt-5.6-sol");
        assert_eq!(model_id("gpt-5.6-terra"), "gpt-5.6-terra");
        assert_eq!(model_id("auto"), "auto");
        assert_eq!(public_name("claude-opus-4.8"), "claude-opus-4-8");
        assert_eq!(public_name("claude-sonnet-5"), "claude-sonnet-5");
        assert_eq!(public_name("gpt-5.6-sol"), "gpt-5.6-sol");
        assert_eq!(supports_effort("claude-opus-5"), Some("output_config"));
        assert_eq!(supports_effort("claude-opus-4.8"), Some("output_config"));
        assert_eq!(supports_effort("claude-sonnet-4.5"), None);
        assert_eq!(supports_effort("claude-haiku-4.5"), None);
        assert_eq!(supports_effort("gpt-5.6-sol"), Some("reasoning"));
        assert_eq!(arn_region("arn:aws:codewhisperer:us-east-1:772420518308:profile/KPC9UG7QMPKD"), Some("us-east-1"));
        assert!(valid_region("ap-southeast-1") && !valid_region("evil.host/x"));
    }

    fn request() -> Request {
        Request {
            system: vec!["Be brief.".into()],
            messages: vec![
                Message { role: Role::User, parts: vec![Part::Text("hi".into())] },
                Message {
                    role: Role::Assistant,
                    parts: vec![Part::ToolCall {
                        id: "c1".into(),
                        name: "read".into(),
                        args: "{\"p\":1}".into(),
                        sig: None,
                    }],
                },
                Message {
                    role: Role::User,
                    parts: vec![Part::ToolResult {
                        id: "c1".into(),
                        name: Some("read".into()),
                        content: vec![Part::Text("ok".into())],
                        is_error: false,
                    }],
                },
            ],
            tools: vec![crate::ir::Tool {
                name: "read".into(),
                description: String::new(),
                parameters: json!({"type":"object","properties":{"p":{"type":"integer"}},"required":[],"additionalProperties":false}),
            }],
            ..Default::default()
        }
    }

    #[test]
    fn the_latest_tool_call_stays_structured() {
        let body = build_request(&request(), "claude-opus-4-8");
        let cs = &body["conversationState"];
        let history = cs["history"].as_array().unwrap();
        assert_eq!(history.len(), 4);
        assert_eq!(history[0]["userInputMessage"]["content"], "Be brief.");
        assert_eq!(history[1]["assistantResponseMessage"]["content"], PRIMING_REPLY);
        assert_eq!(history[2]["userInputMessage"]["content"], "hi");
        assert_eq!(history[3]["assistantResponseMessage"]["toolUses"][0]["toolUseId"], "c1");
        let current = &cs["currentMessage"]["userInputMessage"];
        assert_eq!(current["modelId"], "claude-opus-4.8");
        assert_eq!(current["content"], ".");
        let ctx = &current["userInputMessageContext"];
        assert_eq!(ctx["toolResults"][0]["toolUseId"], "c1");
        assert_eq!(ctx["toolResults"][0]["content"][0]["text"], "ok");
        let spec = &ctx["tools"][0]["toolSpecification"];
        assert_eq!(spec["description"], "Tool: read");
        assert!(spec["inputSchema"]["json"].get("required").is_none());
        assert!(spec["inputSchema"]["json"].get("additionalProperties").is_none());
        assert!(body.get("additionalModelRequestFields").is_none());
        assert_eq!(body["inferenceConfig"]["maxTokens"], DEFAULT_MAX_TOKENS);
    }

    #[test]
    fn older_tool_results_are_told_as_text() {
        let mut req = request();
        req.messages.push(Message { role: Role::Assistant, parts: vec![Part::Text("done".into())] });
        req.messages.push(Message { role: Role::User, parts: vec![Part::Text("thanks".into())] });
        let body = build_request(&req, "claude-sonnet-5");
        let history = body["conversationState"]["history"].as_array().unwrap();
        assert!(history[3]["assistantResponseMessage"].get("toolUses").is_none());
        let narrated = history[4]["userInputMessage"]["content"].as_str().unwrap();
        assert!(narrated.starts_with("Tool results:"), "{narrated}");
        assert!(narrated.contains("[read] ok"));
        assert!(history[4]["userInputMessage"].get("userInputMessageContext").is_none());
        assert_eq!(body["conversationState"]["currentMessage"]["userInputMessage"]["content"], "thanks");
    }

    #[test]
    fn thinking_and_effort_go_where_kiro_reads_them() {
        let mut req = request();
        req.reasoning = Some(crate::ir::Reasoning { effort: Some("max".into()), ..Default::default() });
        let body = build_request(&req, "claude-opus-5");
        let system = body["conversationState"]["history"][0]["userInputMessage"]["content"].as_str().unwrap();
        assert!(system.starts_with("<thinking_mode>enabled</thinking_mode>\n<max_thinking_length>120000"));
        assert!(system.ends_with("Be brief."));
        assert_eq!(body["additionalModelRequestFields"]["output_config"]["effort"], "max");
        let body = build_request(&req, "gpt-5.6-sol");
        assert_eq!(body["additionalModelRequestFields"]["reasoning"]["effort"], "max");
        // Models without the knob get the prompt only.
        let body = build_request(&req, "claude-sonnet-4-5");
        assert!(body.get("additionalModelRequestFields").is_none());
        let mut plain = request();
        plain.reasoning = Some(crate::ir::Reasoning { disabled: true, ..Default::default() });
        let body = build_request(&plain, "claude-opus-5-thinking");
        assert!(
            body["conversationState"]["history"][0]["userInputMessage"]["content"]
                .as_str()
                .unwrap()
                .contains("thinking_mode")
        );
        assert_eq!(body["conversationState"]["currentMessage"]["userInputMessage"]["modelId"], "claude-opus-5");
    }

    #[test]
    fn conversation_ids_are_stable_for_one_conversation() {
        let a = conversation_id("m", "s", "first");
        assert_eq!(a, conversation_id("m", "s", "first"));
        assert_ne!(a, conversation_id("m", "s", "other"));
        assert_eq!(uuid::Uuid::parse_str(&a).unwrap().get_version_num(), 5);
    }

    #[test]
    fn frames_decode_to_events() {
        let mut bytes = frame(
            &[
                (":event-type", "initial-response"),
                (":content-type", "application/x-amz-json-1.0"),
                (":message-type", "event"),
            ],
            br#"{"conversationId":""}"#,
        );
        bytes.extend(event("assistantResponseEvent", r#"{"content":"Hel"}"#));
        bytes.extend(event("assistantResponseEvent", r#"{"content":"lo"}"#));
        bytes.extend(event("toolUseEvent", r#"{"toolUseId":"t1","name":"read","input":"{\"p\":"}"#));
        bytes.extend(event("toolUseEvent", r#"{"toolUseId":"t1","name":"read","input":"1}","stop":true}"#));
        bytes.extend(event("metadataEvent", r#"{"stopReason":"END_TURN"}"#));
        bytes.extend(event("contextUsageEvent", r#"{"contextUsagePercentage":1.0}"#));
        bytes.extend(event("meteringEvent", r#"{"unit":"credit","usage":0.5}"#));
        let events = decode(&bytes, "claude-opus-4.8");
        let mut agg = crate::ir::Aggregate::default();
        events.iter().for_each(|e| agg.push(e));
        assert_eq!(agg.text(), "Hello");
        assert!(
            matches!(&agg.parts[1], Part::ToolCall { id, name, args, .. } if id == "t1" && name == "read" && args == "{\"p\":1}")
        );
        assert_eq!(agg.usage.input, 10_000);
        assert_eq!(agg.finish_reason(), Finish::ToolCalls);
    }

    #[test]
    fn whole_object_tool_input_is_sent_once() {
        let mut bytes = event("toolUseEvent", r#"{"toolUseId":"t1","name":"read","input":{"p":1}}"#);
        bytes.extend(event("toolUseEvent", r#"{"toolUseId":"t1","name":"read","input":{"p":1,"q":2},"stop":true}"#));
        let events = decode(&bytes, "claude-sonnet-5");
        let args: Vec<&str> = events
            .iter()
            .filter_map(|e| if let Event::ToolArgs { delta, .. } = e { Some(delta.as_str()) } else { None })
            .collect();
        assert_eq!(args, vec![r#"{"p":1,"q":2}"#]);
    }

    #[test]
    fn thinking_tags_split_across_frames_become_reasoning() {
        let mut bytes = event("assistantResponseEvent", r#"{"content":"<thin"}"#);
        bytes.extend(event("assistantResponseEvent", r#"{"content":"king>I wonder</thi"}"#));
        bytes.extend(event("assistantResponseEvent", r#"{"content":"nking>Answer"}"#));
        bytes.extend(event("metadataEvent", r#"{"stopReason":"END_TURN"}"#));
        let events = decode(&bytes, "claude-sonnet-5");
        let mut agg = crate::ir::Aggregate::default();
        events.iter().for_each(|e| agg.push(e));
        assert_eq!(agg.reasoning_text(), "I wonder");
        assert_eq!(agg.text(), "Answer");
        assert_eq!(agg.finish_reason(), Finish::Stop);
    }

    #[test]
    fn exceptions_and_corruption_are_errors() {
        let bytes = frame(
            &[(":message-type", "exception"), (":exception-type", "ThrottlingException")],
            br#"{"message":"Too many requests"}"#,
        );
        let events = decode(&bytes, "claude-sonnet-5");
        assert!(matches!(&events[0], Event::Error { status: 429, message } if message.contains("Too many requests")));
        let mut bad = event("assistantResponseEvent", r#"{"content":"x"}"#);
        let last = bad.len() - 1;
        bad[last] ^= 0xff;
        assert!(next_frame(&mut BytesMut::from(&bad[..])).is_err());
        let empty = decode(&event("metadataEvent", r#"{"stopReason":"END_TURN"}"#), "claude-sonnet-5");
        assert!(matches!(&empty[0], Event::Error { status: 502, .. }));
    }

    #[test]
    fn usage_limits_become_a_monthly_window() {
        let v = json!({
            "usageBreakdownList": [{"resourceType":"CREDIT","currentUsage":5736,"usageLimit":10000,
                "currentUsageWithPrecision":5736.4,"usageLimitWithPrecision":10000.0}],
            "nextDateReset": 1793491200,
            "subscriptionInfo": {"subscriptionTitle":"KIRO POWER","subscriptionType":"POWER"},
            "userInfo": {"email":"a@b.c"}
        });
        let (w, plan) = windows(&v);
        assert_eq!(plan.as_deref(), Some("KIRO POWER"));
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].name, "month");
        assert!((w[0].used - 57.364).abs() < 0.01);
        assert_eq!(w[0].resets_at.unwrap().timestamp(), 1793491200);
        assert!(windows(&json!({})).0.is_empty());
    }
}
