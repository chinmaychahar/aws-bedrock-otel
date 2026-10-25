//! Opt-in capture of prompts and responses, as JSON following the GenAI semantic conventions'
//! message schemas.
//!
//! Media sent as bytes is recorded without the bytes; media stored in S3 is recorded by URI.

use std::collections::BTreeMap;

use aws_sdk_bedrockruntime::types::{
    AudioSource, CitationGeneratedContent, ContentBlock, ConversationRole, DocumentContentBlock,
    DocumentFormat, DocumentSource, GuardrailConverseContentBlock, ImageSource, Message,
    ReasoningContentBlock, SystemContentBlock, Tool, ToolConfiguration, ToolResultContentBlock,
    VideoSource,
};
use aws_smithy_types::{Document, Number};
use serde_json::{Map, Value as Json, json};

/// What to capture, when capture is on.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ContentCapture {
    /// Maximum length in bytes of each captured text and JSON value.
    pub(crate) max_length: Option<usize>,
}

impl ContentCapture {
    fn text(&self, text: &str) -> Json {
        let text = match self.max_length {
            Some(max) if text.len() > max => {
                let mut end = max;
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                &text[..end]
            }
            _ => text,
        };
        Json::String(text.to_owned())
    }

    /// `value`, or its JSON text cut to the maximum length if that is longer.
    fn json(&self, value: Json) -> Json {
        match self.max_length {
            Some(max) => {
                let text = value.to_string();
                if text.len() > max {
                    self.text(&text)
                } else {
                    value
                }
            }
            None => value,
        }
    }

    fn text_part(&self, text: &str) -> Json {
        json!({ "type": "text", "content": self.text(text) })
    }

    /// `gen_ai.system_instructions`, or `None` if there are none.
    pub(crate) fn system_instructions(&self, blocks: &[SystemContentBlock]) -> Option<String> {
        let parts: Vec<Json> = blocks
            .iter()
            .filter_map(|block| match block {
                SystemContentBlock::Text(text) => Some(self.text_part(text)),
                SystemContentBlock::GuardContent(GuardrailConverseContentBlock::Text(text)) => {
                    Some(self.text_part(text.text()))
                }
                _ => None,
            })
            .collect();
        (!parts.is_empty()).then(|| Json::Array(parts).to_string())
    }

    /// `gen_ai.input.messages`, or `None` if there are none.
    pub(crate) fn input_messages(&self, messages: &[Message]) -> Option<String> {
        let messages: Vec<Json> = messages.iter().map(|m| self.message(m)).collect();
        (!messages.is_empty()).then(|| Json::Array(messages).to_string())
    }

    /// `gen_ai.output.messages` for a single response message.
    pub(crate) fn output_messages(&self, message: &Message) -> String {
        Json::Array(vec![self.message(message)]).to_string()
    }

    fn message(&self, message: &Message) -> Json {
        let parts: Vec<Json> = message
            .content()
            .iter()
            .filter_map(|block| self.part(block))
            .collect();
        json!({ "role": role(message.role()), "parts": parts })
    }

    fn part(&self, block: &ContentBlock) -> Option<Json> {
        Some(match block {
            ContentBlock::Text(text) => self.text_part(text),
            ContentBlock::Image(image) => {
                let uri = match image.source()? {
                    ImageSource::S3Location(location) => Some(location.uri()),
                    _ => None,
                };
                media(
                    "image",
                    Some(format!("image/{}", image.format().as_str())),
                    uri,
                )
            }
            ContentBlock::Video(video) => {
                let uri = match video.source()? {
                    VideoSource::S3Location(location) => Some(location.uri()),
                    _ => None,
                };
                media("video", video_mime_type(video.format().as_str()), uri)
            }
            ContentBlock::Audio(audio) => {
                let uri = match audio.source()? {
                    AudioSource::S3Location(location) => Some(location.uri()),
                    _ => None,
                };
                media("audio", audio_mime_type(audio.format().as_str()), uri)
            }
            ContentBlock::Document(document) => {
                let mime_type = document_mime_type(document.format());
                match document.source()? {
                    DocumentSource::S3Location(location) => {
                        media("document", mime_type, Some(location.uri()))
                    }
                    DocumentSource::Text(text) => self.text_part(text),
                    DocumentSource::Content(blocks) => {
                        let text: String = blocks
                            .iter()
                            .filter_map(|block| match block {
                                DocumentContentBlock::Text(text) => Some(text.as_str()),
                                _ => None,
                            })
                            .collect();
                        if text.is_empty() {
                            media("document", mime_type, None)
                        } else {
                            self.text_part(&text)
                        }
                    }
                    _ => media("document", mime_type, None),
                }
            }
            ContentBlock::ToolUse(tool_use) => json!({
                "type": "tool_call",
                "id": tool_use.tool_use_id(),
                "name": tool_use.name(),
                "arguments": self.json(document_to_json(tool_use.input())),
            }),
            ContentBlock::ToolResult(result) => {
                let mut responses: Vec<Json> = result
                    .content()
                    .iter()
                    .filter_map(|content| self.tool_result(content))
                    .collect();
                let response = if responses.len() == 1 {
                    responses.remove(0)
                } else {
                    Json::Array(responses)
                };
                json!({
                    "type": "tool_call_response",
                    "id": result.tool_use_id(),
                    "response": response,
                })
            }
            ContentBlock::ReasoningContent(ReasoningContentBlock::ReasoningText(reasoning)) => {
                json!({ "type": "reasoning", "content": self.text(reasoning.text()) })
            }
            ContentBlock::GuardContent(GuardrailConverseContentBlock::Text(text)) => {
                self.text_part(text.text())
            }
            ContentBlock::CitationsContent(citations) => {
                let text: String = citations
                    .content()
                    .iter()
                    .filter_map(|content| match content {
                        CitationGeneratedContent::Text(text) => Some(text.as_str()),
                        _ => None,
                    })
                    .collect();
                self.text_part(&text)
            }
            _ => return None,
        })
    }

    fn tool_result(&self, content: &ToolResultContentBlock) -> Option<Json> {
        Some(match content {
            ToolResultContentBlock::Text(text) => self.text(text),
            ToolResultContentBlock::Json(document) => self.json(document_to_json(document)),
            ToolResultContentBlock::Image(image) => media(
                "image",
                Some(format!("image/{}", image.format().as_str())),
                None,
            ),
            ToolResultContentBlock::Document(document) => {
                media("document", document_mime_type(document.format()), None)
            }
            ToolResultContentBlock::Video(video) => {
                media("video", video_mime_type(video.format().as_str()), None)
            }
            _ => return None,
        })
    }
}

/// `gen_ai.tool.definitions`, with only the required `type` and `name` of each tool.
pub(crate) fn tool_definitions(config: Option<&ToolConfiguration>) -> Option<String> {
    let tools: Vec<Json> = config?
        .tools()
        .iter()
        .filter_map(|tool| match tool {
            Tool::ToolSpec(spec) => Some(json!({ "type": "function", "name": spec.name() })),
            Tool::SystemTool(tool) => Some(json!({ "type": "system", "name": tool.name() })),
            _ => None,
        })
        .collect();
    (!tools.is_empty()).then(|| Json::Array(tools).to_string())
}

fn role(role: &ConversationRole) -> &str {
    match role {
        ConversationRole::User => "user",
        ConversationRole::Assistant => "assistant",
        ConversationRole::System => "system",
        other => other.as_str(),
    }
}

/// A media part: a `uri` part for S3 objects, otherwise a placeholder without the bytes.
fn media(modality: &str, mime_type: Option<String>, uri: Option<&str>) -> Json {
    let mut part = Map::new();
    match uri {
        Some(uri) => {
            part.insert("type".into(), json!("uri"));
            part.insert("modality".into(), json!(modality));
            part.insert("uri".into(), json!(uri));
        }
        None => {
            part.insert("type".into(), json!(modality));
        }
    }
    if let Some(mime_type) = mime_type {
        part.insert("mime_type".into(), json!(mime_type));
    }
    Json::Object(part)
}

fn document_mime_type(format: &DocumentFormat) -> Option<String> {
    let mime_type = match format.as_str() {
        "pdf" => "application/pdf",
        "csv" => "text/csv",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "html" => "text/html",
        "txt" => "text/plain",
        "md" => "text/markdown",
        _ => return None,
    };
    Some(mime_type.to_owned())
}

fn video_mime_type(format: &str) -> Option<String> {
    let mime_type = match format {
        "mp4" => "video/mp4",
        "mov" => "video/quicktime",
        "mkv" => "video/x-matroska",
        "webm" => "video/webm",
        "flv" => "video/x-flv",
        "mpeg" | "mpg" => "video/mpeg",
        "wmv" => "video/x-ms-wmv",
        "three_gp" => "video/3gpp",
        _ => return None,
    };
    Some(mime_type.to_owned())
}

fn audio_mime_type(format: &str) -> Option<String> {
    let mime_type = match format {
        "mp3" | "mpeg" | "mpga" => "audio/mpeg",
        "wav" => "audio/wav",
        "flac" => "audio/flac",
        "ogg" => "audio/ogg",
        "opus" => "audio/opus",
        "aac" => "audio/aac",
        "m4a" | "mp4" => "audio/mp4",
        "webm" => "audio/webm",
        "mka" | "mkv" => "audio/x-matroska",
        _ => return None,
    };
    Some(mime_type.to_owned())
}

fn document_to_json(document: &Document) -> Json {
    match document {
        Document::Object(object) => Json::Object(
            object
                .iter()
                .map(|(key, value)| (key.clone(), document_to_json(value)))
                .collect(),
        ),
        Document::Array(array) => Json::Array(array.iter().map(document_to_json).collect()),
        Document::Number(Number::PosInt(value)) => json!(value),
        Document::Number(Number::NegInt(value)) => json!(value),
        Document::Number(Number::Float(value)) => json!(value),
        Document::String(value) => json!(value),
        Document::Bool(value) => json!(value),
        Document::Null => Json::Null,
    }
}

/// The assistant message of a `ConverseStream` response, rebuilt from its events.
#[derive(Debug)]
pub(crate) struct StreamedMessage {
    blocks: BTreeMap<u64, StreamedBlock>,
    /// Text past this length is not kept, as it would be cut anyway.
    max_length: Option<usize>,
}

/// Appends `chunk` to `buffer` while `buffer` is shorter than `max_length`.
fn append(buffer: &mut String, chunk: &str, max_length: Option<usize>) {
    if max_length.is_none_or(|max| buffer.len() < max) {
        buffer.push_str(chunk);
    }
}

#[derive(Debug)]
enum StreamedBlock {
    Text(String),
    Reasoning(String),
    ToolUse {
        id: String,
        name: String,
        /// `None` until the first input delta.
        input: Option<String>,
    },
    /// The result of a tool that Bedrock runs, such as a system tool.
    ToolResult {
        id: String,
        text: String,
        json: Vec<Json>,
        /// Bytes of JSON kept, to stop keeping more past the maximum length.
        json_length: usize,
    },
    Image {
        mime_type: Option<String>,
    },
}

impl StreamedMessage {
    pub(crate) fn new(max_length: Option<usize>) -> Self {
        Self {
            blocks: BTreeMap::new(),
            max_length,
        }
    }

    /// Handles a `contentBlockStart` payload.
    pub(crate) fn start(&mut self, payload: &[u8]) {
        let Ok(event) = serde_json::from_slice::<Json>(payload) else {
            return;
        };
        let Some(index) = event["contentBlockIndex"].as_u64() else {
            return;
        };
        let start = &event["start"];
        let string = |value: &Json| value.as_str().unwrap_or_default().to_owned();
        let block = if start["toolUse"].is_object() {
            StreamedBlock::ToolUse {
                id: string(&start["toolUse"]["toolUseId"]),
                name: string(&start["toolUse"]["name"]),
                input: None,
            }
        } else if start["toolResult"].is_object() {
            StreamedBlock::ToolResult {
                id: string(&start["toolResult"]["toolUseId"]),
                text: String::new(),
                json: Vec::new(),
                json_length: 0,
            }
        } else if start["image"].is_object() {
            StreamedBlock::Image {
                mime_type: start["image"]["format"]
                    .as_str()
                    .map(|format| format!("image/{format}")),
            }
        } else {
            return;
        };
        self.blocks.insert(index, block);
    }

    /// Handles a `contentBlockDelta` payload.
    pub(crate) fn delta(&mut self, payload: &[u8]) {
        let Ok(event) = serde_json::from_slice::<Json>(payload) else {
            return;
        };
        let Some(index) = event["contentBlockIndex"].as_u64() else {
            return;
        };
        let max_length = self.max_length;
        let delta = &event["delta"];
        if let Some(text) = delta["text"].as_str() {
            if let StreamedBlock::Text(content) = self
                .blocks
                .entry(index)
                .or_insert_with(|| StreamedBlock::Text(String::new()))
            {
                append(content, text, max_length);
            }
        } else if let Some(text) = delta["reasoningContent"]["text"].as_str() {
            if let StreamedBlock::Reasoning(content) = self
                .blocks
                .entry(index)
                .or_insert_with(|| StreamedBlock::Reasoning(String::new()))
            {
                append(content, text, max_length);
            }
        } else if let Some(chunk) = delta["toolUse"]["input"].as_str()
            && let Some(StreamedBlock::ToolUse { input, .. }) = self.blocks.get_mut(&index)
        {
            append(input.get_or_insert_default(), chunk, max_length);
        } else if let Some(items) = delta["toolResult"].as_array()
            && let Some(StreamedBlock::ToolResult {
                text,
                json,
                json_length,
                ..
            }) = self.blocks.get_mut(&index)
        {
            for item in items {
                if let Some(chunk) = item["text"].as_str() {
                    append(text, chunk, max_length);
                } else if !item["json"].is_null() && max_length.is_none_or(|max| *json_length < max)
                {
                    *json_length += item["json"].to_string().len();
                    json.push(item["json"].clone());
                }
            }
        }
    }

    /// `gen_ai.output.messages`, or `None` if nothing was received.
    pub(crate) fn output_messages(&self, capture: &ContentCapture) -> Option<String> {
        if self.blocks.is_empty() {
            return None;
        }
        let parts: Vec<Json> = self
            .blocks
            .values()
            .map(|block| match block {
                StreamedBlock::Text(text) => capture.text_part(text),
                StreamedBlock::Reasoning(text) => {
                    json!({ "type": "reasoning", "content": capture.text(text) })
                }
                StreamedBlock::ToolUse { id, name, input } => {
                    // Arguments arrive as JSON text in pieces; keep them as text if incomplete.
                    let arguments = match input {
                        None => json!({}),
                        Some(input) => serde_json::from_str::<Json>(input)
                            .map(|arguments| capture.json(arguments))
                            .unwrap_or_else(|_| capture.text(input)),
                    };
                    json!({ "type": "tool_call", "id": id, "name": name, "arguments": arguments })
                }
                StreamedBlock::ToolResult { id, text, json, .. } => {
                    let mut responses: Vec<Json> = json
                        .iter()
                        .map(|value| capture.json(value.clone()))
                        .collect();
                    if !text.is_empty() {
                        responses.insert(0, capture.text(text));
                    }
                    let response = if responses.len() == 1 {
                        responses.remove(0)
                    } else {
                        Json::Array(responses)
                    };
                    json!({ "type": "tool_call_response", "id": id, "response": response })
                }
                StreamedBlock::Image { mime_type } => media("image", mime_type.clone(), None),
            })
            .collect();
        Some(json!([{ "role": "assistant", "parts": parts }]).to_string())
    }
}
