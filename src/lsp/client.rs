//! A language server process and the JSON-RPC connection to it.
//!
//! Blocking pipe I/O runs on a reader and a writer thread per server. Messages
//! reach the main thread through a channel, where they are parsed into their
//! types and dispatched, so the interface never waits on a server.

use std::cell::{Ref, RefCell};
use std::collections::HashMap;
use std::fmt;
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::rc::{Rc, Weak};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use futures_channel::{mpsc as async_mpsc, oneshot};
use futures_util::StreamExt;
use gtk::prelude::*;
use gtk::{gio, glib};
use lsp_types::notification::{self, Notification};
use lsp_types::request::{self, Request};
use lsp_types::{
    ClientCapabilities, ClientInfo, CompletionClientCapabilities, CompletionItemCapability,
    CompletionItemCapabilityResolveSupport, ConfigurationParams, GeneralClientCapabilities,
    GotoCapability, HoverClientCapabilities, InitializeParams, MarkupKind, MessageType,
    NumberOrString, PositionEncodingKind, ProgressParams, ProgressParamsValue,
    PublishDiagnosticsClientCapabilities, PublishDiagnosticsParams, SemanticTokenModifier,
    SemanticTokenType, SemanticTokensClientCapabilities, SemanticTokensClientCapabilitiesRequests,
    SemanticTokensFullOptions, SemanticTokensWorkspaceClientCapabilities, ServerCapabilities,
    ShowMessageParams, TextDocumentClientCapabilities, TextDocumentSyncClientCapabilities,
    TokenFormat, Uri, WindowClientCapabilities, WorkDoneProgress, WorkspaceClientCapabilities,
    WorkspaceFolder,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

use super::document::Document;
use super::environment::Environment;
use super::status::{Busy, Level, Status};
use super::{Encoding, Server, SharedEnvironment};
use crate::display_name;

/// How long a server must have run for a crash to be worth a restart, rather
/// than a sign that it will crash again.
const MIN_UPTIME_FOR_RESTART: Duration = Duration::from_secs(30);

/// How long a server gets to shut down before it is killed.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

/// The kinds of token that servers can mark out for semantic highlighting.
/// Servers can add their own, and the theme can style any.
const TOKEN_TYPES: [SemanticTokenType; 23] = [
    SemanticTokenType::NAMESPACE,
    SemanticTokenType::TYPE,
    SemanticTokenType::CLASS,
    SemanticTokenType::ENUM,
    SemanticTokenType::INTERFACE,
    SemanticTokenType::STRUCT,
    SemanticTokenType::TYPE_PARAMETER,
    SemanticTokenType::PARAMETER,
    SemanticTokenType::VARIABLE,
    SemanticTokenType::PROPERTY,
    SemanticTokenType::ENUM_MEMBER,
    SemanticTokenType::EVENT,
    SemanticTokenType::FUNCTION,
    SemanticTokenType::METHOD,
    SemanticTokenType::MACRO,
    SemanticTokenType::KEYWORD,
    SemanticTokenType::MODIFIER,
    SemanticTokenType::COMMENT,
    SemanticTokenType::STRING,
    SemanticTokenType::NUMBER,
    SemanticTokenType::REGEXP,
    SemanticTokenType::OPERATOR,
    SemanticTokenType::DECORATOR,
];

const TOKEN_MODIFIERS: [SemanticTokenModifier; 10] = [
    SemanticTokenModifier::DECLARATION,
    SemanticTokenModifier::DEFINITION,
    SemanticTokenModifier::READONLY,
    SemanticTokenModifier::STATIC,
    SemanticTokenModifier::DEPRECATED,
    SemanticTokenModifier::ABSTRACT,
    SemanticTokenModifier::ASYNC,
    SemanticTokenModifier::MODIFICATION,
    SemanticTokenModifier::DOCUMENTATION,
    SemanticTokenModifier::DEFAULT_LIBRARY,
];

pub(super) struct Client {
    server: &'static Server,
    /// The project folder, if the server is for one.
    root: Option<gio::File>,
    status: Rc<Status>,
    /// The server's command line and environment, once found.
    launch: RefCell<Option<Launch>>,
    state: RefCell<State>,
    connection: RefCell<Option<Connection>>,
    /// Open documents by URI, which the server's notifications refer to.
    documents: RefCell<HashMap<String, Weak<Document>>>,
    /// The server's work in progress, by its token, showing in the status.
    works: RefCell<HashMap<NumberOrString, Busy>>,
}

struct Launch {
    program: PathBuf,
    command: &'static [&'static str],
    environment: Rc<Environment>,
}

enum State {
    Starting,
    Running(Box<ServerCapabilities>),
    ShuttingDown,
    Stopped,
}

struct Connection {
    child: Child,
    outgoing: mpsc::Sender<Vec<u8>>,
    pending: HashMap<i32, oneshot::Sender<Response>>,
    next_id: i32,
    started: Instant,
}

impl Drop for Connection {
    fn drop(&mut self) {
        // Both are no-ops for a process that has already been reaped.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

type Response = Result<Box<RawValue>, ResponseError>;

#[derive(Debug)]
pub(super) enum Error {
    Disconnected,
    Server(ResponseError),
    Invalid(serde_json::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Disconnected => f.write_str("the language server stopped"),
            Self::Server(err) => f.write_str(&err.message),
            Self::Invalid(err) => write!(f, "invalid response: {err}"),
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub(super) struct ResponseError {
    code: i64,
    message: String,
}

/// A message from a server, parsed no further than its envelope.
#[derive(Deserialize)]
struct Incoming {
    id: Option<NumberOrString>,
    method: Option<String>,
    params: Option<Box<RawValue>>,
    result: Option<Box<RawValue>>,
    error: Option<ResponseError>,
}

#[derive(Serialize)]
struct Outgoing<'a, T> {
    jsonrpc: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<NumberOrString>,
    #[serde(skip_serializing_if = "Option::is_none")]
    method: Option<&'a str>,
    #[serde(flatten)]
    body: Body<T>,
}

#[derive(Serialize)]
#[serde(rename_all = "lowercase")]
enum Body<T> {
    Params(T),
    Result(T),
    Error(ResponseError),
}

impl Client {
    /// Starts `server` for the project folder `root`, or for files on their
    /// own, in `environment` once it is loaded, telling `status` of what
    /// becomes of it, what it says, and its work.
    pub(super) fn start(
        server: &'static Server,
        root: Option<&gio::File>,
        environment: SharedEnvironment,
        status: Rc<Status>,
    ) -> Rc<Self> {
        let client = Rc::new(Self {
            server,
            root: root.cloned(),
            status,
            launch: RefCell::default(),
            state: RefCell::new(State::Starting),
            connection: RefCell::default(),
            documents: RefCell::default(),
            works: RefCell::default(),
        });
        glib::spawn_future_local(Rc::clone(&client).launch(environment));
        client
    }

    async fn launch(self: Rc<Self>, environment: SharedEnvironment) {
        let environment = environment.await;
        // Shut down while the environment loaded.
        if !matches!(*self.state.borrow(), State::Starting) {
            return;
        }
        let Some((program, command)) = environment.find(self.server.commands) else {
            let missing = self.server.not_found(environment.path().as_deref());
            self.status.log(self.name(), Level::Error, &missing);
            self.state.replace(State::Stopped);
            return;
        };
        self.launch.replace(Some(Launch {
            program,
            command,
            environment,
        }));
        self.connect();
    }

    fn name(&self) -> &'static str {
        self.launch
            .borrow()
            .as_ref()
            .map_or(self.server.name(), |launch| launch.command[0])
    }

    pub(super) fn is_stopped(&self) -> bool {
        matches!(*self.state.borrow(), State::Stopped)
    }

    /// The server's capabilities, while it is running.
    pub(super) fn capabilities(&self) -> Option<Ref<'_, ServerCapabilities>> {
        Ref::filter_map(self.state.borrow(), |state| match state {
            State::Running(capabilities) => Some(&**capabilities),
            _ => None,
        })
        .ok()
    }

    pub(super) fn encoding(&self) -> Encoding {
        self.capabilities().map_or(Encoding::Utf16, |capabilities| {
            Encoding::new(capabilities.position_encoding.as_ref())
        })
    }

    pub(super) fn documents(&self) -> Vec<Rc<Document>> {
        self.documents
            .borrow()
            .values()
            .filter_map(Weak::upgrade)
            .collect()
    }

    /// Tracks `document`, which is opened on the server once it is running.
    pub(super) fn add(&self, uri: &Uri, document: &Rc<Document>) {
        self.documents
            .borrow_mut()
            .insert(uri.as_str().to_owned(), Rc::downgrade(document));
        if self.capabilities().is_some() {
            document.open();
        }
    }

    pub(super) fn remove(&self, uri: &Uri) {
        self.documents.borrow_mut().remove(uri.as_str());
    }

    /// Starts the server process and initializes it, or reports why not.
    fn connect(self: &Rc<Self>) {
        if let Err(err) = self.spawn() {
            let failed = format!("Could not start: {err}");
            self.status.log(self.name(), Level::Error, &failed);
            self.state.replace(State::Stopped);
            return;
        }
        self.state.replace(State::Starting);
        glib::spawn_future_local(Rc::clone(self).initialize());
    }

    fn spawn(self: &Rc<Self>) -> io::Result<()> {
        let mut command = {
            let launch = self.launch.borrow();
            let launch = launch.as_ref().expect("servers launch before connecting");
            let mut command = Command::new(&launch.program);
            command
                .args(&launch.command[1..])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit());
            launch.environment.apply(&mut command);
            command
        };
        // Without a project, in an empty folder, as servers such as
        // rust-analyzer look for one in their working folder instead.
        let dir = match self.root.as_ref().and_then(gio::File::path) {
            Some(dir) => dir,
            None => empty_dir()?,
        };
        command.current_dir(dir);
        let mut child = command.spawn()?;
        let stdin = child.stdin.take().expect("stdin is piped");
        let stdout = child.stdout.take().expect("stdout is piped");
        let (outgoing, outbox) = mpsc::channel();
        let (inbox, mut incoming) = async_mpsc::unbounded();
        // Owning the process, this kills it if a thread fails to start, which
        // ends the other.
        let connection = Connection {
            child,
            outgoing,
            pending: HashMap::new(),
            next_id: 0,
            started: Instant::now(),
        };
        let thread = |role| std::thread::Builder::new().name(format!("{} {role}", self.name()));
        thread("writer").spawn(move || write_messages(stdin, outbox))?;
        thread("reader").spawn(move || read_messages(stdout, &inbox))?;
        self.connection.replace(Some(connection));

        let client = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            while let Some(message) = incoming.next().await {
                let Some(client) = client.upgrade() else {
                    return;
                };
                client.dispatch(message);
            }
            if let Some(client) = client.upgrade() {
                client.disconnected();
            }
        });
        Ok(())
    }

    async fn initialize(self: Rc<Self>) {
        match self
            .request::<request::Initialize>(self.initialize_params())
            .await
        {
            // Unless shut down while the answer came.
            Ok(result) if matches!(*self.state.borrow(), State::Starting) => {
                self.notify::<notification::Initialized>(lsp_types::InitializedParams {});
                self.state
                    .replace(State::Running(Box::new(result.capabilities)));
                self.status.started(self.name());
                for document in self.documents() {
                    document.open();
                }
            }
            Ok(_) | Err(Error::Disconnected) => {}
            Err(err) => {
                let failed = format!("Failed to start: {err}");
                self.status.log(self.name(), Level::Error, &failed);
                self.stop();
            }
        }
    }

    // `root_uri` is deprecated in favour of workspace folders, but some
    // servers still read only it.
    #[allow(deprecated)]
    fn initialize_params(&self) -> InitializeParams {
        let folder = self.workspace_folder();
        let text_document = TextDocumentClientCapabilities {
            synchronization: Some(TextDocumentSyncClientCapabilities {
                did_save: Some(true),
                ..Default::default()
            }),
            publish_diagnostics: Some(PublishDiagnosticsClientCapabilities {
                version_support: Some(true),
                ..Default::default()
            }),
            completion: Some(CompletionClientCapabilities {
                completion_item: Some(CompletionItemCapability {
                    snippet_support: Some(true),
                    documentation_format: Some(vec![MarkupKind::PlainText]),
                    // Imports are costly to find for every item.
                    resolve_support: Some(CompletionItemCapabilityResolveSupport {
                        properties: vec!["additionalTextEdits".to_owned()],
                    }),
                    ..Default::default()
                }),
                context_support: Some(true),
                ..Default::default()
            }),
            hover: Some(HoverClientCapabilities {
                content_format: Some(vec![MarkupKind::PlainText]),
                ..Default::default()
            }),
            definition: Some(GotoCapability {
                link_support: Some(true),
                ..Default::default()
            }),
            semantic_tokens: Some(SemanticTokensClientCapabilities {
                requests: SemanticTokensClientCapabilitiesRequests {
                    range: Some(false),
                    full: Some(SemanticTokensFullOptions::Delta { delta: Some(true) }),
                },
                token_types: TOKEN_TYPES.to_vec(),
                token_modifiers: TOKEN_MODIFIERS.to_vec(),
                formats: vec![TokenFormat::RELATIVE],
                overlapping_token_support: Some(false),
                multiline_token_support: Some(false),
                // Syntax highlighting stays, and tokens the theme leaves be
                // keep its colors.
                augments_syntax_tokens: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        };
        InitializeParams {
            process_id: Some(std::process::id()),
            root_uri: folder.as_ref().map(|folder| folder.uri.clone()),
            workspace_folders: folder.map(|folder| vec![folder]),
            capabilities: ClientCapabilities {
                general: Some(GeneralClientCapabilities {
                    position_encodings: Some(vec![
                        PositionEncodingKind::UTF8,
                        PositionEncodingKind::UTF16,
                    ]),
                    ..Default::default()
                }),
                text_document: Some(text_document),
                workspace: Some(WorkspaceClientCapabilities {
                    workspace_folders: Some(true),
                    configuration: Some(true),
                    semantic_tokens: Some(SemanticTokensWorkspaceClientCapabilities {
                        refresh_support: Some(true),
                    }),
                    ..Default::default()
                }),
                window: Some(WindowClientCapabilities {
                    work_done_progress: Some(true),
                    ..Default::default()
                }),
                ..Default::default()
            },
            client_info: Some(ClientInfo {
                name: "Craft".to_owned(),
                version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            }),
            ..Default::default()
        }
    }

    fn workspace_folder(&self) -> Option<WorkspaceFolder> {
        self.root.as_ref().map(|root| WorkspaceFolder {
            uri: root.uri().parse().expect("GIO produces valid URIs"),
            name: display_name(root),
        })
    }

    /// Sends a request and waits for its response. Dropping the future
    /// cancels the request.
    pub(super) async fn request<R: Request>(&self, params: R::Params) -> Result<R::Result, Error>
    where
        R::Result: DeserializeOwned,
    {
        let (sender, response) = oneshot::channel();
        let id = {
            let mut connection = self.connection.borrow_mut();
            let connection = connection.as_mut().ok_or(Error::Disconnected)?;
            connection.next_id += 1;
            connection.pending.insert(connection.next_id, sender);
            connection.next_id
        };
        let _pending = Pending { client: self, id };
        self.send(
            NumberOrString::Number(id).into(),
            Some(R::METHOD),
            Body::Params(params),
        );
        let result = response.await.map_err(|_| Error::Disconnected)?;
        let result = result.map_err(Error::Server)?;
        serde_json::from_str(result.get()).map_err(Error::Invalid)
    }

    pub(super) fn notify<N: Notification>(&self, params: N::Params) {
        self.send(None, Some(N::METHOD), Body::Params(params));
    }

    fn send<T: Serialize>(&self, id: Option<NumberOrString>, method: Option<&str>, body: Body<T>) {
        if let Some(connection) = &*self.connection.borrow() {
            let message = Outgoing {
                jsonrpc: "2.0",
                id,
                method,
                body,
            };
            let bytes = serde_json::to_vec(&message).expect("messages serialize");
            // Fails only once the writer has stopped, as the reader soon will.
            let _ = connection.outgoing.send(bytes);
        }
    }

    fn dispatch(&self, message: Incoming) {
        match (message.id, message.method) {
            (Some(id), Some(method)) => self.answer(id, &method, message.params.as_deref()),
            (None, Some(method)) => self.handle(&method, message.params.as_deref()),
            (Some(NumberOrString::Number(id)), None) => {
                let sender = self
                    .connection
                    .borrow_mut()
                    .as_mut()
                    .and_then(|connection| connection.pending.remove(&id));
                if let Some(sender) = sender {
                    let _ = sender.send(match message.error {
                        Some(error) => Err(error),
                        None => Ok(message.result.unwrap_or_else(null)),
                    });
                }
            }
            _ => {}
        }
    }

    /// Answers a request from the server.
    fn answer(&self, id: NumberOrString, method: &str, params: Option<&RawValue>) {
        let id = Some(id);
        match method {
            request::WorkspaceConfiguration::METHOD => {
                // No settings: each item gets the server's defaults.
                let items = parse::<ConfigurationParams>(params).map_or(0, |p| p.items.len());
                self.send(id, None, Body::Result(vec![(); items]));
            }
            request::WorkspaceFoldersRequest::METHOD => {
                let folders = self.workspace_folder().map(|folder| [folder]);
                self.send(id, None, Body::Result(folders));
            }
            // Such as once the server has loaded the project.
            request::SemanticTokensRefresh::METHOD => {
                self.send(id, None, Body::Result(()));
                for document in self.documents() {
                    document.rehighlight();
                }
            }
            // Accepted, though nothing comes of them.
            request::WorkDoneProgressCreate::METHOD
            | request::RegisterCapability::METHOD
            | request::UnregisterCapability::METHOD
            | request::ShowMessageRequest::METHOD
            | request::WorkspaceDiagnosticRefresh::METHOD
            | request::InlayHintRefreshRequest::METHOD
            | request::InlineValueRefreshRequest::METHOD
            | request::CodeLensRefresh::METHOD => self.send(id, None, Body::Result(())),
            _ => self.send::<()>(
                id,
                None,
                Body::Error(ResponseError {
                    code: -32601,
                    message: format!("{method} is not supported"),
                }),
            ),
        }
    }

    fn handle(&self, method: &str, params: Option<&RawValue>) {
        match method {
            notification::PublishDiagnostics::METHOD => {
                if let Some(params) = parse(params) {
                    self.publish(params);
                }
            }
            // Both carry a type and a message, such as failing to load the
            // project, which leaves the server running but of little use.
            notification::ShowMessage::METHOD | notification::LogMessage::METHOD => {
                if let Some(params) = parse::<ShowMessageParams>(params)
                    && let Some(level) = level(params.typ)
                {
                    self.status.log(self.name(), level, &params.message);
                }
            }
            notification::Progress::METHOD => {
                if let Some(params) = parse(params) {
                    self.progress(params);
                }
            }
            _ => {}
        }
    }

    /// Follows the server's report on its work.
    fn progress(&self, params: ProgressParams) {
        let ProgressParamsValue::WorkDone(progress) = params.value;
        let mut works = self.works.borrow_mut();
        match progress {
            WorkDoneProgress::Begin(begin) => {
                let work = self.status.busy(self.name(), &begin.title);
                work.report(begin.message, begin.percentage);
                works.insert(params.token, work);
            }
            WorkDoneProgress::Report(report) => {
                if let Some(work) = works.get(&params.token) {
                    work.report(report.message, report.percentage);
                }
            }
            WorkDoneProgress::End(_) => {
                works.remove(&params.token);
            }
        }
    }

    fn publish(&self, params: PublishDiagnosticsParams) {
        // Servers may escape URIs differently; GIO's form is the key.
        let uri = gio::File::for_uri(params.uri.as_str()).uri();
        let document = self
            .documents
            .borrow()
            .get(uri.as_str())
            .and_then(Weak::upgrade);
        if let Some(document) = document {
            document.set_diagnostics(params.version, params.diagnostics);
        }
    }

    /// Handles the server's exit, restarting it if it had been running long
    /// enough to have probably crashed on something passing.
    fn disconnected(self: &Rc<Self>) {
        let started = self.disconnect();
        if matches!(*self.state.borrow(), State::ShuttingDown | State::Stopped) {
            return;
        }
        if started.is_some_and(|started| started.elapsed() >= MIN_UPTIME_FOR_RESTART) {
            let restarting = "Stopped unexpectedly; restarting it";
            self.status.log(self.name(), Level::Warning, restarting);
            self.connect();
        } else {
            self.status
                .log(self.name(), Level::Error, "Stopped unexpectedly");
            self.state.replace(State::Stopped);
        }
    }

    fn stop(&self) {
        self.state.replace(State::Stopped);
        self.disconnect();
    }

    /// Ends the connection, killing the server if it is still running, and
    /// forgets what the server had: the documents and its work in progress.
    /// Returns when the connection began.
    fn disconnect(&self) -> Option<Instant> {
        // Dropping the connection reaps the process and fails pending requests.
        let started = self.connection.take().map(|connection| connection.started);
        for document in self.documents() {
            document.closed();
        }
        self.works.take();
        started
    }

    /// Asks the server to exit, killing it if it does not in time.
    pub(super) async fn shutdown(self: Rc<Self>) {
        let running = self.capabilities().is_some();
        self.state.replace(State::ShuttingDown);
        if running {
            let shutdown = self.request::<request::Shutdown>(());
            if let Ok(Ok(())) = glib::future_with_timeout(SHUTDOWN_GRACE, shutdown).await {
                self.notify::<notification::Exit>(());
                let deadline = Instant::now() + SHUTDOWN_GRACE;
                while Instant::now() < deadline && self.has_exited() == Some(false) {
                    glib::timeout_future(Duration::from_millis(20)).await;
                }
            }
        }
        self.stop();
    }

    fn has_exited(&self) -> Option<bool> {
        let mut connection = self.connection.borrow_mut();
        Some(matches!(
            connection.as_mut()?.child.try_wait(),
            Ok(Some(_)) | Err(_)
        ))
    }
}

/// Removes a request from the pending ones when its future completes or is
/// dropped, telling the server in the latter case.
struct Pending<'a> {
    client: &'a Client,
    id: i32,
}

impl Drop for Pending<'_> {
    fn drop(&mut self) {
        let unanswered = self
            .client
            .connection
            .borrow_mut()
            .as_mut()
            .and_then(|connection| connection.pending.remove(&self.id))
            .is_some();
        if unanswered {
            self.client
                .notify::<notification::Cancel>(lsp_types::CancelParams {
                    id: NumberOrString::Number(self.id),
                });
        }
    }
}

/// A folder of Craft's own for servers to run in, holding no project: empty,
/// and kept so, being read-only.
fn empty_dir() -> io::Result<PathBuf> {
    let dir = glib::user_runtime_dir().join("craft");
    std::fs::create_dir_all(&dir)?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500))?;
    Ok(dir)
}

/// How much a server's message matters, if it is worth logging at all.
fn level(typ: MessageType) -> Option<Level> {
    match typ {
        MessageType::ERROR => Some(Level::Error),
        MessageType::WARNING => Some(Level::Warning),
        MessageType::INFO => Some(Level::Info),
        _ => None,
    }
}

fn parse<T: DeserializeOwned>(params: Option<&RawValue>) -> Option<T> {
    serde_json::from_str(params.map_or("null", RawValue::get)).ok()
}

fn null() -> Box<RawValue> {
    RawValue::from_string("null".to_owned()).expect("null is JSON")
}

fn write_messages(stdin: ChildStdin, outbox: mpsc::Receiver<Vec<u8>>) {
    let mut stdin = BufWriter::new(stdin);
    for message in outbox {
        let written = write!(stdin, "Content-Length: {}\r\n\r\n", message.len())
            .and_then(|()| stdin.write_all(&message))
            .and_then(|()| stdin.flush());
        if written.is_err() {
            return;
        }
    }
}

fn read_messages(stdout: ChildStdout, inbox: &async_mpsc::UnboundedSender<Incoming>) {
    let mut stdout = BufReader::new(stdout);
    while let Ok(Some(body)) = read_message(&mut stdout) {
        match serde_json::from_slice(&body) {
            Ok(message) => {
                if inbox.unbounded_send(message).is_err() {
                    return;
                }
            }
            Err(err) => eprintln!("craft: ignoring invalid message from server: {err}"),
        }
    }
}

/// Reads the body of the next message, or `None` at the end of the stream.
fn read_message(reader: &mut impl BufRead) -> io::Result<Option<Vec<u8>>> {
    let mut length = None;
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        let header = line.trim_end();
        if header.is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = Some(value.trim().parse().map_err(io::Error::other)?);
        }
    }
    let length = length.ok_or_else(|| io::Error::other("message without Content-Length"))?;
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    Ok(Some(body))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_framed_messages() {
        let mut stream = io::Cursor::new(
            "Content-Length: 2\r\n\r\n{}content-length: 4\r\nContent-Type: x\r\n\r\nnull",
        );
        assert_eq!(read_message(&mut stream).unwrap().unwrap(), b"{}");
        assert_eq!(read_message(&mut stream).unwrap().unwrap(), b"null");
        assert!(read_message(&mut stream).unwrap().is_none());
    }

    #[test]
    fn rejects_messages_without_length() {
        let mut stream = io::Cursor::new("Content-Type: x\r\n\r\n{}");
        assert!(read_message(&mut stream).is_err());
    }

    #[test]
    fn writes_requests_responses_and_errors() {
        let request = Outgoing {
            jsonrpc: "2.0",
            id: Some(NumberOrString::Number(1)),
            method: Some("shutdown"),
            body: Body::Params(()),
        };
        assert_eq!(
            serde_json::to_string(&request).unwrap(),
            r#"{"jsonrpc":"2.0","id":1,"method":"shutdown","params":null}"#
        );
        let response = Outgoing {
            jsonrpc: "2.0",
            id: Some(NumberOrString::String("a".into())),
            method: None,
            body: Body::Result([(); 2]),
        };
        assert_eq!(
            serde_json::to_string(&response).unwrap(),
            r#"{"jsonrpc":"2.0","id":"a","result":[null,null]}"#
        );
        let error = Outgoing::<()> {
            jsonrpc: "2.0",
            id: Some(NumberOrString::Number(2)),
            method: None,
            body: Body::Error(ResponseError {
                code: -32601,
                message: "no".into(),
            }),
        };
        assert_eq!(
            serde_json::to_string(&error).unwrap(),
            r#"{"jsonrpc":"2.0","id":2,"error":{"code":-32601,"message":"no"}}"#
        );
    }
}
