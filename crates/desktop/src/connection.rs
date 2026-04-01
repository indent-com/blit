use std::collections::HashMap;
use tokio::sync::mpsc;

use crate::remotes::RemoteConfig;
use crate::transport::{connect_remote, read_frame, write_frame};

#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub pty_id: u16,
    pub tag: String,
    pub command: String,
    pub title: Option<String>,
    pub exit_status: Option<i32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SessionKey {
    pub remote: String,
    pub pty_id: u16,
}

impl std::fmt::Display for SessionKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.remote, self.pty_id)
    }
}

#[derive(Clone, Debug)]
pub enum ConnectionStatus {
    Connecting,
    Connected,
    Disconnected(String),
}

pub enum Command {
    Input { pty_id: u16, data: Vec<u8> },
    Resize { pty_id: u16, rows: u16, cols: u16 },
    Focus(u16),
    CreateSession { tag: String, command: Option<String>, rows: u16, cols: u16 },
    CloseSession(u16),
    RestartSession(u16),
    KillSession { pty_id: u16, signal: i32 },
    Scroll { pty_id: u16, offset: u32 },
    Subscribe(u16),
    Unsubscribe(u16),
    Search { query: String },
    Mouse { pty_id: u16, kind: u8, button: u8, col: u16, row: u16 },
    DisplayRate(u16),
}

#[derive(Debug)]
pub enum ServerEvent {
    Hello { version: u16, features: u32 },
    SessionList(Vec<SessionInfo>),
    SessionCreated { pty_id: u16, tag: String },
    SessionCreatedN { nonce: u16, pty_id: u16, tag: String },
    SessionClosed(u16),
    SessionExited { pty_id: u16, exit_status: i32 },
    FrameUpdate { pty_id: u16, payload: Vec<u8> },
    TitleChanged { pty_id: u16, title: String },
    SearchResults { request_id: u16, results: Vec<SearchResult> },
    StatusChanged(ConnectionStatus),
    ReconnectNeeded,
    Ready,
}

#[derive(Debug)]
pub struct SearchResult {
    pub pty_id: u16,
    pub score: u32,
    pub context: String,
}

pub struct ConnectionHandle {
    pub cmd_tx: mpsc::UnboundedSender<Command>,
    pub status: ConnectionStatus,
    pub remote: RemoteConfig,
}

pub struct ConnectionManager {
    pub event_tx: mpsc::UnboundedSender<(String, ServerEvent)>,
    pub event_rx: mpsc::UnboundedReceiver<(String, ServerEvent)>,
    pub connections: HashMap<String, ConnectionHandle>,
}

impl ConnectionManager {
    pub fn new() -> Self {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        Self {
            event_tx,
            event_rx,
            connections: HashMap::new(),
        }
    }

    pub fn connect(&mut self, remote: RemoteConfig, hub: &str) {
        let name = remote.name.clone();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let event_tx = self.event_tx.clone();
        let remote_clone = remote.clone();
        let hub = hub.to_string();

        self.connections.insert(
            name.clone(),
            ConnectionHandle {
                cmd_tx,
                status: ConnectionStatus::Connecting,
                remote,
            },
        );

        tokio::spawn(connection_task(name, remote_clone, hub, cmd_rx, event_tx));
    }

    #[allow(dead_code)]
    pub fn disconnect(&mut self, name: &str) {
        self.connections.remove(name);
    }

    pub fn send(&self, remote: &str, cmd: Command) {
        if let Some(handle) = self.connections.get(remote) {
            let _ = handle.cmd_tx.send(cmd);
        }
    }

    #[allow(dead_code)]
    pub fn send_to_session(&self, key: &SessionKey, cmd: Command) {
        self.send(&key.remote, cmd);
    }

    pub fn update_status(&mut self, remote: &str, status: ConnectionStatus) {
        if let Some(handle) = self.connections.get_mut(remote) {
            handle.status = status;
        }
    }
}

async fn connection_task(
    name: String,
    remote: RemoteConfig,
    hub: String,
    mut cmd_rx: mpsc::UnboundedReceiver<Command>,
    event_tx: mpsc::UnboundedSender<(String, ServerEvent)>,
) {
    let _ = event_tx.send((name.clone(), ServerEvent::StatusChanged(ConnectionStatus::Connecting)));

    let transport = match connect_remote(&remote, &hub).await {
        Ok(t) => {
            let _ = event_tx.send((name.clone(), ServerEvent::StatusChanged(ConnectionStatus::Connected)));
            t
        }
        Err(e) => {
            let _ = event_tx.send((
                name.clone(),
                ServerEvent::StatusChanged(ConnectionStatus::Disconnected(e)),
            ));
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            let _ = event_tx.send((name.clone(), ServerEvent::ReconnectNeeded));
            return;
        }
    };

    let (mut reader, mut writer) = transport.split();

    let display_rate_msg = blit_remote::msg_display_rate(120);
    let _ = write_frame(&mut writer, &display_rate_msg).await;

    let name_r = name.clone();
    let event_tx_r = event_tx.clone();
    let reader_task = tokio::spawn(async move {
        loop {
            let frame = match read_frame(&mut reader).await {
                Some(f) => f,
                None => break,
            };
            if frame.is_empty() {
                continue;
            }
            if let Some(evt) = parse_server_frame(&frame) {
                if event_tx_r.send((name_r.clone(), evt)).is_err() {
                    break;
                }
            }
        }
    });

    let writer_task = tokio::spawn(async move {
        while let Some(cmd) = cmd_rx.recv().await {
            let msg = command_to_bytes(&cmd);
            if !write_frame(&mut writer, &msg).await {
                break;
            }
        }
    });

    tokio::select! {
        _ = reader_task => {},
        _ = writer_task => {},
    }

    let _ = event_tx.send((
        name.clone(),
        ServerEvent::StatusChanged(ConnectionStatus::Disconnected("connection lost".into())),
    ));

    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    let _ = event_tx.send((name.clone(), ServerEvent::ReconnectNeeded));
}

fn parse_server_frame(data: &[u8]) -> Option<ServerEvent> {
    match blit_remote::parse_server_msg(data) {
        Some(blit_remote::ServerMsg::Hello { version, features }) => {
            Some(ServerEvent::Hello { version, features })
        }
        Some(blit_remote::ServerMsg::Update { pty_id, payload }) => {
            Some(ServerEvent::FrameUpdate {
                pty_id,
                payload: payload.to_vec(),
            })
        }
        Some(blit_remote::ServerMsg::Created { pty_id, tag }) => {
            Some(ServerEvent::SessionCreated {
                pty_id,
                tag: tag.to_string(),
            })
        }
        Some(blit_remote::ServerMsg::CreatedN { nonce, pty_id, tag }) => {
            Some(ServerEvent::SessionCreatedN {
                nonce,
                pty_id,
                tag: tag.to_string(),
            })
        }
        Some(blit_remote::ServerMsg::Closed { pty_id }) => Some(ServerEvent::SessionClosed(pty_id)),
        Some(blit_remote::ServerMsg::Exited { pty_id, exit_status }) => {
            Some(ServerEvent::SessionExited { pty_id, exit_status })
        }
        Some(blit_remote::ServerMsg::List { entries }) => {
            let sessions = entries
                .iter()
                .map(|e| SessionInfo {
                    pty_id: e.pty_id,
                    tag: e.tag.to_string(),
                    command: e.command.to_string(),
                    title: None,
                    exit_status: None,
                })
                .collect();
            Some(ServerEvent::SessionList(sessions))
        }
        Some(blit_remote::ServerMsg::Title { pty_id, title }) => {
            Some(ServerEvent::TitleChanged {
                pty_id,
                title: String::from_utf8_lossy(title).to_string(),
            })
        }
        Some(blit_remote::ServerMsg::SearchResults { request_id, results }) => {
            let results = results
                .iter()
                .map(|r| SearchResult {
                    pty_id: r.pty_id,
                    score: r.score,
                    context: String::from_utf8_lossy(r.context).to_string(),
                })
                .collect();
            Some(ServerEvent::SearchResults { request_id, results })
        }
        Some(blit_remote::ServerMsg::Ready) => Some(ServerEvent::Ready),
        _ => None,
    }
}

fn command_to_bytes(cmd: &Command) -> Vec<u8> {
    match cmd {
        Command::Input { pty_id, data } => blit_remote::msg_input(*pty_id, data),
        Command::Resize { pty_id, rows, cols } => blit_remote::msg_resize(*pty_id, *rows, *cols),
        Command::Focus(pty_id) => blit_remote::msg_focus(*pty_id),
        Command::CreateSession { tag, command, rows, cols } => {
            let nonce = rand_nonce();
            if let Some(cmd_str) = command {
                blit_remote::msg_create_n_command(nonce, *rows, *cols, tag, cmd_str)
            } else {
                blit_remote::msg_create_n(nonce, *rows, *cols, tag)
            }
        }
        Command::CloseSession(pty_id) => blit_remote::msg_close(*pty_id),
        Command::RestartSession(pty_id) => blit_remote::msg_restart(*pty_id),
        Command::KillSession { pty_id, signal } => blit_remote::msg_kill(*pty_id, *signal),
        Command::Scroll { pty_id, offset } => blit_remote::msg_scroll(*pty_id, *offset),
        Command::Subscribe(pty_id) => blit_remote::msg_subscribe(*pty_id),
        Command::Unsubscribe(pty_id) => blit_remote::msg_unsubscribe(*pty_id),
        Command::Search { query } => {
            let request_id = rand_nonce();
            blit_remote::msg_search(request_id, query)
        }
        Command::Mouse { pty_id, kind, button, col, row } => {
            let mut msg = Vec::with_capacity(8);
            msg.push(blit_remote::C2S_MOUSE);
            msg.extend_from_slice(&pty_id.to_le_bytes());
            msg.push(*kind);
            msg.push(*button);
            msg.extend_from_slice(&col.to_le_bytes());
            msg.extend_from_slice(&row.to_le_bytes());
            msg
        }
        Command::DisplayRate(fps) => blit_remote::msg_display_rate(*fps),
    }
}

fn rand_nonce() -> u16 {
    use std::sync::atomic::{AtomicU16, Ordering};
    static NONCE: AtomicU16 = AtomicU16::new(1);
    NONCE.fetch_add(1, Ordering::Relaxed)
}
