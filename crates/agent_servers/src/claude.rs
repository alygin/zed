mod mcp_server;
mod tools;

use collections::HashMap;
use project::Project;
use std::cell::RefCell;
use std::fmt::Display;
use std::path::Path;
use std::rc::Rc;

use agentic_coding_protocol::{
    self as acp, AnyAgentRequest, AnyAgentResult, Client, ProtocolVersion,
    StreamAssistantMessageChunkParams, ToolCallContent, UpdateToolCallParams,
};
use anyhow::{Context as _, Result, anyhow};
use futures::channel::oneshot;
use futures::future::LocalBoxFuture;
use futures::{AsyncBufReadExt, AsyncWriteExt};
use futures::{
    AsyncRead, AsyncWrite, FutureExt, StreamExt,
    channel::mpsc::{self, UnboundedReceiver, UnboundedSender},
    io::BufReader,
    select_biased,
};
use gpui::{App, AppContext, Entity, Task};
use serde::{Deserialize, Serialize};
use util::ResultExt;

use crate::claude::mcp_server::ClaudeMcpServer;
use crate::claude::tools::ClaudeTool;
use crate::{AgentServer, find_bin_in_path};
use acp_thread::{AcpClientDelegate, AcpThread, AgentConnection};

#[derive(Clone)]
pub struct ClaudeCode;

impl AgentServer for ClaudeCode {
    fn name(&self) -> &'static str {
        "Claude Code"
    }

    fn empty_state_headline(&self) -> &'static str {
        self.name()
    }

    fn empty_state_message(&self) -> &'static str {
        ""
    }

    fn logo(&self) -> ui::IconName {
        ui::IconName::AiClaude
    }

    fn supports_always_allow(&self) -> bool {
        false
    }

    fn new_thread(
        &self,
        root_dir: &Path,
        project: &Entity<Project>,
        cx: &mut App,
    ) -> Task<Result<Entity<AcpThread>>> {
        let project = project.clone();
        let root_dir = root_dir.to_path_buf();
        let title = self.name().into();
        cx.spawn(async move |cx| {
            let (mut delegate_tx, delegate_rx) = watch::channel(None);
            let tool_id_map = Rc::new(RefCell::new(HashMap::default()));

            let permission_mcp_server =
                ClaudeMcpServer::new(delegate_rx, tool_id_map.clone(), cx).await?;

            let mut mcp_servers = HashMap::default();
            mcp_servers.insert(
                mcp_server::SERVER_NAME.to_string(),
                permission_mcp_server.server_config()?,
            );
            let mcp_config = McpConfig { mcp_servers };

            let mcp_config_file = tempfile::NamedTempFile::new()?;
            let (mcp_config_file, mcp_config_path) = mcp_config_file.into_parts();

            let mut mcp_config_file = smol::fs::File::from(mcp_config_file);
            mcp_config_file
                .write_all(serde_json::to_string(&mcp_config)?.as_bytes())
                .await?;
            mcp_config_file.flush().await?;

            let command = find_bin_in_path("claude", &project, cx)
                .await
                .context("Failed to find claude binary")?;

            let mut child = util::command::new_smol_command(&command)
                .args([
                    "--input-format",
                    "stream-json",
                    "--output-format",
                    "stream-json",
                    "--print",
                    "--verbose",
                    "--mcp-config",
                    mcp_config_path.to_string_lossy().as_ref(),
                    "--permission-prompt-tool",
                    &format!(
                        "mcp__{}__{}",
                        mcp_server::SERVER_NAME,
                        mcp_server::PERMISSION_TOOL
                    ),
                    "--allowedTools",
                    "mcp__zed__Read,mcp__zed__Edit",
                    "--disallowedTools",
                    "Read,Edit",
                ])
                .current_dir(root_dir)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::inherit())
                .kill_on_drop(true)
                .spawn()?;

            let stdin = child.stdin.take().unwrap();
            let stdout = child.stdout.take().unwrap();

            let (incoming_message_tx, mut incoming_message_rx) = mpsc::unbounded();
            let (outgoing_tx, outgoing_rx) = mpsc::unbounded();

            let io_task =
                ClaudeAgentConnection::handle_io(outgoing_rx, incoming_message_tx, stdin, stdout);
            cx.background_spawn(async move {
                io_task.await.log_err();
                drop(mcp_config_path);
                drop(child);
            })
            .detach();

            cx.new(|cx| {
                let end_turn_tx = Rc::new(RefCell::new(None));
                let delegate = AcpClientDelegate::new(cx.entity().downgrade(), cx.to_async());
                delegate_tx.send(Some(delegate.clone())).log_err();

                let handler_task = cx.foreground_executor().spawn({
                    let end_turn_tx = end_turn_tx.clone();
                    let tool_id_map = tool_id_map.clone();
                    async move {
                        while let Some(message) = incoming_message_rx.next().await {
                            ClaudeAgentConnection::handle_message(
                                delegate.clone(),
                                message,
                                end_turn_tx.clone(),
                                tool_id_map.clone(),
                            )
                            .await
                        }
                    }
                });

                let mut connection = ClaudeAgentConnection {
                    outgoing_tx,
                    end_turn_tx,
                    _handler_task: handler_task,
                    _mcp_server: None,
                };

                connection._mcp_server = Some(permission_mcp_server);
                acp_thread::AcpThread::new(connection, title, None, project.clone(), cx)
            })
        })
    }
}

impl AgentConnection for ClaudeAgentConnection {
    /// Send a request to the agent and wait for a response.
    fn request_any(
        &self,
        params: AnyAgentRequest,
    ) -> LocalBoxFuture<'static, Result<acp::AnyAgentResult>> {
        let end_turn_tx = self.end_turn_tx.clone();
        let outgoing_tx = self.outgoing_tx.clone();
        async move {
            match params {
                // todo: consider sending an empty request so we get the init response?
                AnyAgentRequest::InitializeParams(_) => Ok(AnyAgentResult::InitializeResponse(
                    acp::InitializeResponse {
                        is_authenticated: true,
                        protocol_version: ProtocolVersion::latest(),
                    },
                )),
                AnyAgentRequest::AuthenticateParams(_) => {
                    Err(anyhow!("Authentication not supported"))
                }
                AnyAgentRequest::SendUserMessageParams(message) => {
                    let (tx, rx) = oneshot::channel();
                    end_turn_tx.borrow_mut().replace(tx);
                    let mut content = String::new();
                    for chunk in message.chunks {
                        match chunk {
                            agentic_coding_protocol::UserMessageChunk::Text { text } => {
                                content.push_str(&text)
                            }
                            agentic_coding_protocol::UserMessageChunk::Path { path } => {
                                content.push_str(&format!("@{path:?}"))
                            }
                        }
                    }
                    outgoing_tx.unbounded_send(SdkMessage::User {
                        message: Message {
                            role: Role::User,
                            content: Content::UntaggedText(content),
                            id: None,
                            model: None,
                            stop_reason: None,
                            stop_sequence: None,
                            usage: None,
                        },
                        session_id: None,
                    })?;
                    rx.await??;
                    Ok(AnyAgentResult::SendUserMessageResponse(
                        acp::SendUserMessageResponse,
                    ))
                }
                AnyAgentRequest::CancelSendMessageParams(_) => Ok(
                    AnyAgentResult::CancelSendMessageResponse(acp::CancelSendMessageResponse),
                ),
            }
        }
        .boxed_local()
    }
}

struct ClaudeAgentConnection {
    outgoing_tx: UnboundedSender<SdkMessage>,
    end_turn_tx: Rc<RefCell<Option<oneshot::Sender<Result<()>>>>>,
    _mcp_server: Option<ClaudeMcpServer>,
    _handler_task: Task<()>,
}

impl ClaudeAgentConnection {
    async fn handle_message(
        delegate: AcpClientDelegate,
        message: SdkMessage,
        end_turn_tx: Rc<RefCell<Option<oneshot::Sender<Result<()>>>>>,
        tool_id_map: Rc<RefCell<HashMap<String, acp::ToolCallId>>>,
    ) {
        match message {
            SdkMessage::Assistant { message, .. } | SdkMessage::User { message, .. } => {
                for chunk in message.content.chunks() {
                    match chunk {
                        ContentChunk::Text { text } | ContentChunk::UntaggedText(text) => {
                            delegate
                                .stream_assistant_message_chunk(StreamAssistantMessageChunkParams {
                                    chunk: acp::AssistantMessageChunk::Text { text },
                                })
                                .await
                                .log_err();
                        }
                        ContentChunk::ToolUse { id, name, input } => {
                            if let Some(resp) = delegate
                                .push_tool_call(ClaudeTool::infer(&name, input).as_acp())
                                .await
                                .log_err()
                            {
                                tool_id_map.borrow_mut().insert(id, resp.id);
                            }
                        }
                        ContentChunk::ToolResult {
                            content,
                            tool_use_id,
                        } => {
                            let id = tool_id_map.borrow_mut().remove(&tool_use_id);
                            if let Some(id) = id {
                                delegate
                                    .update_tool_call(UpdateToolCallParams {
                                        tool_call_id: id,
                                        status: acp::ToolCallStatus::Finished,
                                        content: Some(ToolCallContent::Markdown {
                                            // For now we only include text content
                                            markdown: content.to_string(),
                                        }),
                                    })
                                    .await
                                    .log_err();
                            }
                        }
                        ContentChunk::Image
                        | ContentChunk::Document
                        | ContentChunk::Thinking
                        | ContentChunk::RedactedThinking
                        | ContentChunk::WebSearchToolResult => {
                            delegate
                                .stream_assistant_message_chunk(StreamAssistantMessageChunkParams {
                                    chunk: acp::AssistantMessageChunk::Text {
                                        text: format!("Unsupported content: {:?}", chunk),
                                    },
                                })
                                .await
                                .log_err();
                        }
                    }
                }
            }
            SdkMessage::Result {
                is_error, subtype, ..
            } => {
                if let Some(end_turn_tx) = end_turn_tx.borrow_mut().take() {
                    if is_error {
                        end_turn_tx.send(Err(anyhow!("Error: {subtype}"))).ok();
                    } else {
                        end_turn_tx.send(Ok(())).ok();
                    }
                }
            }
            SdkMessage::System { .. } => {}
        }
    }

    async fn handle_io(
        mut outgoing_rx: UnboundedReceiver<SdkMessage>,
        incoming_tx: UnboundedSender<SdkMessage>,
        mut outgoing_bytes: impl Unpin + AsyncWrite,
        incoming_bytes: impl Unpin + AsyncRead,
    ) -> Result<()> {
        let mut output_reader = BufReader::new(incoming_bytes);
        let mut outgoing_line = Vec::new();
        let mut incoming_line = String::new();
        loop {
            select_biased! {
                message = outgoing_rx.next() => {
                    if let Some(message) = message {
                        outgoing_line.clear();
                        serde_json::to_writer(&mut outgoing_line, &message)?;
                        log::trace!("send: {}", String::from_utf8_lossy(&outgoing_line));
                        outgoing_line.push(b'\n');
                        outgoing_bytes.write_all(&outgoing_line).await.ok();
                    } else {
                        break;
                    }
                }
                bytes_read = output_reader.read_line(&mut incoming_line).fuse() => {
                    if bytes_read? == 0 {
                        break
                    }
                    log::trace!("recv: {}", &incoming_line);
                    match serde_json::from_str::<SdkMessage>(&incoming_line) {
                        Ok(message) => {
                            incoming_tx.unbounded_send(message).log_err();
                        }
                        Err(error) => {
                            log::error!("failed to parse incoming message: {error}. Raw: {incoming_line}");
                        }
                    }
                    incoming_line.clear();
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Message {
    role: Role,
    content: Content,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stop_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stop_sequence: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    usage: Option<Usage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
enum Content {
    UntaggedText(String),
    Chunks(Vec<ContentChunk>),
}

impl Content {
    pub fn chunks(self) -> impl Iterator<Item = ContentChunk> {
        match self {
            Self::Chunks(chunks) => chunks.into_iter(),
            Self::UntaggedText(text) => vec![ContentChunk::Text { text: text.clone() }].into_iter(),
        }
    }
}

impl Display for Content {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Content::UntaggedText(txt) => write!(f, "{}", txt),
            Content::Chunks(chunks) => {
                for chunk in chunks {
                    write!(f, "{}", chunk)?;
                }
                Ok(())
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ContentChunk {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        content: Content,
        tool_use_id: String,
    },
    // TODO
    Image,
    Document,
    Thinking,
    RedactedThinking,
    WebSearchToolResult,
    #[serde(untagged)]
    UntaggedText(String),
}

impl Display for ContentChunk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ContentChunk::Text { text } => write!(f, "{}", text),
            ContentChunk::UntaggedText(text) => write!(f, "{}", text),
            ContentChunk::ToolResult { content, .. } => write!(f, "{}", content),
            ContentChunk::Image
            | ContentChunk::Document
            | ContentChunk::Thinking
            | ContentChunk::RedactedThinking
            | ContentChunk::ToolUse { .. }
            | ContentChunk::WebSearchToolResult => {
                write!(f, "\n{:?}\n", &self)
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Usage {
    input_tokens: u32,
    cache_creation_input_tokens: u32,
    cache_read_input_tokens: u32,
    output_tokens: u32,
    service_tier: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Role {
    System,
    Assistant,
    User,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MessageParam {
    role: Role,
    content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum SdkMessage {
    // An assistant message
    Assistant {
        message: Message, // from Anthropic SDK
        #[serde(skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
    },

    // A user message
    User {
        message: Message, // from Anthropic SDK
        #[serde(skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
    },

    // Emitted as the last message in a conversation
    Result {
        subtype: ResultErrorType,
        duration_ms: f64,
        duration_api_ms: f64,
        is_error: bool,
        num_turns: i32,
        #[serde(skip_serializing_if = "Option::is_none")]
        result: Option<String>,
        session_id: String,
        total_cost_usd: f64,
    },
    // Emitted as the first message at the start of a conversation
    System {
        cwd: String,
        session_id: String,
        tools: Vec<String>,
        model: String,
        mcp_servers: Vec<McpServer>,
        #[serde(rename = "apiKeySource")]
        api_key_source: String,
        #[serde(rename = "permissionMode")]
        permission_mode: PermissionMode,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ResultErrorType {
    Success,
    ErrorMaxTurns,
    ErrorDuringExecution,
}

impl Display for ResultErrorType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResultErrorType::Success => write!(f, "success"),
            ResultErrorType::ErrorMaxTurns => write!(f, "error_max_turns"),
            ResultErrorType::ErrorDuringExecution => write!(f, "error_during_execution"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct McpServer {
    name: String,
    status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
enum PermissionMode {
    Default,
    AcceptEdits,
    BypassPermissions,
    Plan,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct McpConfig {
    mcp_servers: HashMap<String, McpServerConfig>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct McpServerConfig {
    command: String,
    args: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    env: Option<HashMap<String, String>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_deserialize_content_untagged_text() {
        let json = json!("Hello, world!");
        let content: Content = serde_json::from_value(json).unwrap();
        match content {
            Content::UntaggedText(text) => assert_eq!(text, "Hello, world!"),
            _ => panic!("Expected UntaggedText variant"),
        }
    }

    #[test]
    fn test_deserialize_content_chunks() {
        let json = json!([
            {
                "type": "text",
                "text": "Hello"
            },
            {
                "type": "tool_use",
                "id": "tool_123",
                "name": "calculator",
                "input": {"operation": "add", "a": 1, "b": 2}
            }
        ]);
        let content: Content = serde_json::from_value(json).unwrap();
        match content {
            Content::Chunks(chunks) => {
                assert_eq!(chunks.len(), 2);
                match &chunks[0] {
                    ContentChunk::Text { text } => assert_eq!(text, "Hello"),
                    _ => panic!("Expected Text chunk"),
                }
                match &chunks[1] {
                    ContentChunk::ToolUse { id, name, input } => {
                        assert_eq!(id, "tool_123");
                        assert_eq!(name, "calculator");
                        assert_eq!(input["operation"], "add");
                        assert_eq!(input["a"], 1);
                        assert_eq!(input["b"], 2);
                    }
                    _ => panic!("Expected ToolUse chunk"),
                }
            }
            _ => panic!("Expected Chunks variant"),
        }
    }

    #[test]
    fn test_deserialize_tool_result_untagged_text() {
        let json = json!({
            "type": "tool_result",
            "content": "Result content",
            "tool_use_id": "tool_456"
        });
        let chunk: ContentChunk = serde_json::from_value(json).unwrap();
        match chunk {
            ContentChunk::ToolResult {
                content,
                tool_use_id,
            } => {
                match content {
                    Content::UntaggedText(text) => assert_eq!(text, "Result content"),
                    _ => panic!("Expected UntaggedText content"),
                }
                assert_eq!(tool_use_id, "tool_456");
            }
            _ => panic!("Expected ToolResult variant"),
        }
    }

    #[test]
    fn test_deserialize_tool_result_chunks() {
        let json = json!({
            "type": "tool_result",
            "content": [
                {
                    "type": "text",
                    "text": "Processing complete"
                },
                {
                    "type": "text",
                    "text": "Result: 42"
                }
            ],
            "tool_use_id": "tool_789"
        });
        let chunk: ContentChunk = serde_json::from_value(json).unwrap();
        match chunk {
            ContentChunk::ToolResult {
                content,
                tool_use_id,
            } => {
                match content {
                    Content::Chunks(chunks) => {
                        assert_eq!(chunks.len(), 2);
                        match &chunks[0] {
                            ContentChunk::Text { text } => assert_eq!(text, "Processing complete"),
                            _ => panic!("Expected Text chunk"),
                        }
                        match &chunks[1] {
                            ContentChunk::Text { text } => assert_eq!(text, "Result: 42"),
                            _ => panic!("Expected Text chunk"),
                        }
                    }
                    _ => panic!("Expected Chunks content"),
                }
                assert_eq!(tool_use_id, "tool_789");
            }
            _ => panic!("Expected ToolResult variant"),
        }
    }
}
