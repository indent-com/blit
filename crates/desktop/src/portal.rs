//! xdg-desktop-portal backend interfaces normalized onto the viewer protocol.

use super::{Common, Event, MAX_STRING_BYTES, PortalStream, clip_text, hint_string, strip_markup};
use blit_remote::media::{
    PortalAccessRequest, PortalChoice, PortalChoiceValue, PortalDecision, PortalReply,
    PortalRequest, PortalScreenCastRequest,
};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, oneshot};
use zbus::object_server::{ObjectServer, SignalEmitter};
use zbus::zvariant::{OwnedObjectPath, OwnedValue};
use zbus::{Connection, fdo, interface};

const MAX_PENDING: usize = 32;
const MAX_SESSION_OBJECTS: usize = 32;

enum Pending {
    Access(oneshot::Sender<PortalReply>),
    ScreenCast {
        session_path: String,
        sender: oneshot::Sender<ScreenCastCompletion>,
    },
}

struct ScreenCastCompletion {
    reply: PortalReply,
    session_id: u32,
    streams: Vec<PortalStream>,
}

pub(super) enum ScreenCastDelivery {
    Accepted,
    ReceiverDropped,
    UnknownRequest,
}

#[derive(Default)]
struct ScreenCastSession {
    app_id: String,
    selected: bool,
    multiple: bool,
    start_pending: bool,
    start_attempted: bool,
    server_session_id: Option<u32>,
    created_order: u64,
}

#[derive(Default)]
pub(super) struct State {
    next_id: u32,
    next_session_order: u64,
    pending: HashMap<u32, Pending>,
    sessions: HashMap<String, ScreenCastSession>,
}

impl State {
    fn allocate(&mut self) -> Option<u32> {
        if self.pending.len() >= MAX_PENDING {
            return None;
        }
        loop {
            self.next_id = self.next_id.wrapping_add(1).max(1);
            if !self.pending.contains_key(&self.next_id) {
                return Some(self.next_id);
            }
        }
    }

    fn insert_session(&mut self, path: String, app_id: String) -> Result<Option<String>, ()> {
        if self.sessions.contains_key(&path) {
            return Err(());
        }
        let evicted = if self.sessions.len() >= MAX_SESSION_OBJECTS {
            let evicted = self
                .sessions
                .iter()
                .filter(|(_, session)| {
                    !session.start_pending
                        && !session.start_attempted
                        && session.server_session_id.is_none()
                })
                .min_by_key(|(_, session)| session.created_order)
                .map(|(path, _)| path.clone())
                .ok_or(())?;
            self.sessions.remove(&evicted);
            Some(evicted)
        } else {
            None
        };
        self.next_session_order = self.next_session_order.wrapping_add(1).max(1);
        self.sessions.insert(
            path,
            ScreenCastSession {
                app_id,
                created_order: self.next_session_order,
                ..ScreenCastSession::default()
            },
        );
        Ok(evicted)
    }

    pub(super) fn complete_reply(&mut self, reply: PortalReply) {
        match self.pending.remove(&reply.request_id) {
            Some(Pending::Access(sender)) => {
                let _ = sender.send(reply);
            }
            Some(Pending::ScreenCast {
                session_path,
                sender,
            }) => {
                if let Some(session) = self.sessions.get_mut(&session_path) {
                    session.start_pending = false;
                }
                let _ = sender.send(ScreenCastCompletion {
                    reply,
                    session_id: 0,
                    streams: Vec::new(),
                });
            }
            None => {}
        }
    }

    pub(super) fn complete_screencast(
        &mut self,
        request_id: u32,
        session_id: u32,
        streams: Vec<PortalStream>,
    ) -> ScreenCastDelivery {
        let Some(Pending::ScreenCast {
            session_path,
            sender,
        }) = self.pending.remove(&request_id)
        else {
            return ScreenCastDelivery::UnknownRequest;
        };
        let surface_ids = streams.iter().map(|stream| stream.surface_id).collect();
        if let Some(session) = self.sessions.get_mut(&session_path) {
            session.start_pending = false;
            session.server_session_id = Some(session_id);
        }
        let sent = sender.send(ScreenCastCompletion {
            reply: PortalReply {
                request_id,
                decision: PortalDecision::Grant,
                surface_ids,
                choices: Vec::new(),
            },
            session_id,
            streams,
        });
        if sent.is_err() {
            // Keep the session discoverable by `close_server_session`: the
            // command loop must emit Closed and remove its exported object as
            // well as releasing the newly-created server resources.
            return ScreenCastDelivery::ReceiverDropped;
        }
        ScreenCastDelivery::Accepted
    }

    fn cancel_pending(&mut self, request_id: u32) -> bool {
        let Some(pending) = self.pending.remove(&request_id) else {
            return false;
        };
        let reply = PortalReply {
            request_id,
            decision: PortalDecision::Cancelled,
            surface_ids: Vec::new(),
            choices: Vec::new(),
        };
        match pending {
            Pending::Access(sender) => {
                let _ = sender.send(reply);
            }
            Pending::ScreenCast {
                session_path,
                sender,
            } => {
                if let Some(session) = self.sessions.get_mut(&session_path) {
                    session.start_pending = false;
                }
                let _ = sender.send(ScreenCastCompletion {
                    reply,
                    session_id: 0,
                    streams: Vec::new(),
                });
            }
        }
        true
    }
}

pub(super) struct AccessService {
    pub state: Arc<Mutex<State>>,
    pub common: Common,
    pub timeout: Duration,
}

#[interface(name = "org.freedesktop.impl.portal.Access")]
impl AccessService {
    #[allow(clippy::too_many_arguments)]
    async fn access_dialog(
        &self,
        handle: OwnedObjectPath,
        app_id: &str,
        parent_window: &str,
        title: &str,
        subtitle: &str,
        body: &str,
        options: HashMap<String, OwnedValue>,
        #[zbus(object_server)] object_server: &ObjectServer,
    ) -> fdo::Result<(u32, HashMap<String, OwnedValue>)> {
        let Ok(choices) = access_choices(&options) else {
            return Ok((2, HashMap::new()));
        };
        let (request_id, receiver) = {
            let mut state = self.state.lock().await;
            let Some(request_id) = state.allocate() else {
                return Ok((1, HashMap::new()));
            };
            let (sender, receiver) = oneshot::channel();
            state.pending.insert(request_id, Pending::Access(sender));
            (request_id, receiver)
        };
        let request_object = RequestObject {
            request_id,
            state: self.state.clone(),
            common: self.common.clone(),
        };
        if object_server
            .at(handle.as_str(), request_object)
            .await
            .is_err()
        {
            self.state.lock().await.cancel_pending(request_id);
            return Ok((1, HashMap::new()));
        }
        let request = PortalAccessRequest {
            request_id,
            deadline_ms: self.timeout.as_millis().min(u32::MAX as u128) as u32,
            parent_surface_id: None,
            app_id: plain(app_id, 4 * 1024),
            title: plain(title, 4 * 1024),
            subtitle: plain(subtitle, 4 * 1024),
            body: plain_body(body, 16 * 1024),
            deny_label: plain(
                hint_string(&options, "deny_label").unwrap_or("Deny"),
                4 * 1024,
            ),
            grant_label: plain(
                hint_string(&options, "grant_label").unwrap_or("Grant"),
                4 * 1024,
            ),
            icon_name: plain(hint_string(&options, "icon").unwrap_or(""), 4 * 1024),
            choices: choices.clone(),
        };
        if normalized_size(&request) > 16 * 1024
            || self
                .common
                .send(Event::Portal {
                    request: PortalRequest::Access(request),
                    parent_window: plain(parent_window, 128),
                })
                .await
                .is_err()
        {
            self.state.lock().await.cancel_pending(request_id);
            let _ = object_server
                .remove::<RequestObject, _>(handle.as_str())
                .await;
            return Ok((1, HashMap::new()));
        }
        let reply = tokio::time::timeout(self.timeout, receiver).await;
        self.state.lock().await.pending.remove(&request_id);
        let _ = object_server
            .remove::<RequestObject, _>(handle.as_str())
            .await;
        let Ok(Ok(reply)) = reply else {
            let _ = self.common.send(Event::PortalCancel(request_id)).await;
            return Ok((1, HashMap::new()));
        };
        if reply.decision != PortalDecision::Grant {
            return Ok((1, HashMap::new()));
        }
        let Some(values) = validate_choice_reply(&choices, &reply.choices) else {
            return Ok((1, HashMap::new()));
        };
        let mut results = HashMap::new();
        let value = zbus::zvariant::Value::from(values);
        let value =
            OwnedValue::try_from(value).map_err(|error| fdo::Error::Failed(error.to_string()))?;
        results.insert("choices".to_string(), value);
        Ok((0, results))
    }
}

struct RequestObject {
    request_id: u32,
    state: Arc<Mutex<State>>,
    common: Common,
}

#[interface(name = "org.freedesktop.impl.portal.Request")]
impl RequestObject {
    async fn close(&self) {
        if self.state.lock().await.cancel_pending(self.request_id) {
            let _ = self.common.send(Event::PortalCancel(self.request_id)).await;
        }
    }
}

pub(super) struct ScreenCastService {
    pub state: Arc<Mutex<State>>,
    pub common: Common,
    pub timeout: Duration,
}

impl ScreenCastService {
    /// Abort a backend-owned session and make the terminal state visible to
    /// both the portal frontend and the compositor. Invalid ScreenCast state
    /// transitions close the session; they are not recoverable retries.
    async fn abort_session(
        &self,
        connection: &Connection,
        object_server: &ObjectServer,
        path: &str,
    ) {
        let (session_existed, server_session_id, pending_ids) = {
            let mut state = self.state.lock().await;
            let pending_ids = state
                .pending
                .iter()
                .filter_map(|(&request_id, pending)| match pending {
                    Pending::ScreenCast { session_path, .. } if session_path == path => {
                        Some(request_id)
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            for request_id in &pending_ids {
                state.cancel_pending(*request_id);
            }
            let session = state.sessions.remove(path);
            let server_session_id = session
                .as_ref()
                .and_then(|session| session.server_session_id);
            (session.is_some(), server_session_id, pending_ids)
        };
        for request_id in pending_ids {
            let _ = self.common.send(Event::PortalCancel(request_id)).await;
        }
        if let Some(session_id) = server_session_id {
            let _ = self
                .common
                .send(Event::PortalSessionClosed(session_id))
                .await;
        }
        if session_existed {
            if let Ok(emitter) = SignalEmitter::new(connection, path) {
                let _ = SessionObject::closed(&emitter).await;
            }
            let _ = object_server.remove::<SessionObject, _>(path).await;
        }
    }
}

#[interface(name = "org.freedesktop.impl.portal.ScreenCast")]
impl ScreenCastService {
    async fn create_session(
        &self,
        _handle: OwnedObjectPath,
        session_handle: OwnedObjectPath,
        app_id: &str,
        _options: HashMap<String, OwnedValue>,
        #[zbus(connection)] connection: &Connection,
        #[zbus(object_server)] object_server: &ObjectServer,
    ) -> fdo::Result<(u32, HashMap<String, OwnedValue>)> {
        let path = session_handle.to_string();
        let evicted = {
            let mut state = self.state.lock().await;
            let Ok(evicted) = state.insert_session(path.clone(), plain(app_id, 4 * 1024)) else {
                return Ok((1, HashMap::new()));
            };
            evicted
        };
        if let Some(evicted) = evicted {
            if let Ok(emitter) = SignalEmitter::new(connection, evicted.as_str()) {
                let _ = SessionObject::closed(&emitter).await;
            }
            let _ = object_server
                .remove::<SessionObject, _>(evicted.as_str())
                .await;
        }
        let object = SessionObject {
            path: path.clone(),
            state: self.state.clone(),
            common: self.common.clone(),
        };
        if object_server.at(path.as_str(), object).await.is_err() {
            self.state.lock().await.sessions.remove(&path);
            return Ok((1, HashMap::new()));
        }
        Ok((0, HashMap::new()))
    }

    async fn select_sources(
        &self,
        _handle: OwnedObjectPath,
        session_handle: OwnedObjectPath,
        app_id: &str,
        options: HashMap<String, OwnedValue>,
        #[zbus(connection)] connection: &Connection,
        #[zbus(object_server)] object_server: &ObjectServer,
    ) -> fdo::Result<(u32, HashMap<String, OwnedValue>)> {
        let path = session_handle.as_str();
        let parsed_options = (
            option_u32_or(&options, "types", 1),
            option_u32_or(&options, "cursor_mode", 1),
            option_bool_or(&options, "multiple", false),
            option_u32_or(&options, "persist_mode", 0),
            valid_restore_data(&options),
        );
        let (Ok(types), Ok(cursor_mode), Ok(multiple), Ok(persist_mode), true) = parsed_options
        else {
            self.abort_session(connection, object_server, path).await;
            return Ok((2, HashMap::new()));
        };
        let valid = {
            let mut state = self.state.lock().await;
            let Some(session) = state.sessions.get_mut(path) else {
                return Ok((1, HashMap::new()));
            };
            if types != 2
                || cursor_mode != 1
                || persist_mode > 2
                || session.app_id != plain(app_id, 4 * 1024)
                || session.selected
                || session.start_attempted
                || session.server_session_id.is_some()
            {
                false
            } else {
                session.selected = true;
                session.multiple = multiple;
                true
            }
        };
        if !valid {
            self.abort_session(connection, object_server, path).await;
            return Ok((1, HashMap::new()));
        }
        Ok((0, HashMap::new()))
    }

    #[allow(clippy::too_many_arguments)]
    async fn start(
        &self,
        handle: OwnedObjectPath,
        session_handle: OwnedObjectPath,
        app_id: &str,
        parent_window: &str,
        _options: HashMap<String, OwnedValue>,
        #[zbus(connection)] connection: &Connection,
        #[zbus(object_server)] object_server: &ObjectServer,
    ) -> fdo::Result<(u32, HashMap<String, OwnedValue>)> {
        let session_path = session_handle.to_string();
        enum Preparation {
            Missing,
            Invalid,
            Ready {
                request_id: u32,
                receiver: oneshot::Receiver<ScreenCastCompletion>,
                multiple: bool,
                app_id: String,
            },
        }
        let preparation = {
            let mut state = self.state.lock().await;
            let normalized_app_id = plain(app_id, 4 * 1024);
            match state.sessions.get(&session_path) {
                None => Preparation::Missing,
                Some(session)
                    if !session.selected
                        || session.start_pending
                        || session.start_attempted
                        || session.server_session_id.is_some()
                        || session.app_id != normalized_app_id =>
                {
                    Preparation::Invalid
                }
                Some(session) => {
                    let multiple = session.multiple;
                    if let Some(request_id) = state.allocate() {
                        let (sender, receiver) = oneshot::channel();
                        let session = state
                            .sessions
                            .get_mut(&session_path)
                            .expect("session checked above");
                        session.start_pending = true;
                        session.start_attempted = true;
                        state.pending.insert(
                            request_id,
                            Pending::ScreenCast {
                                session_path: session_path.clone(),
                                sender,
                            },
                        );
                        Preparation::Ready {
                            request_id,
                            receiver,
                            multiple,
                            app_id: normalized_app_id,
                        }
                    } else {
                        Preparation::Invalid
                    }
                }
            }
        };
        let (request_id, receiver, multiple, normalized_app_id) = match preparation {
            Preparation::Missing => return Ok((1, HashMap::new())),
            Preparation::Invalid => {
                self.abort_session(connection, object_server, &session_path)
                    .await;
                return Ok((1, HashMap::new()));
            }
            Preparation::Ready {
                request_id,
                receiver,
                multiple,
                app_id,
            } => (request_id, receiver, multiple, app_id),
        };
        let request_object = RequestObject {
            request_id,
            state: self.state.clone(),
            common: self.common.clone(),
        };
        if object_server
            .at(handle.as_str(), request_object)
            .await
            .is_err()
        {
            self.state.lock().await.cancel_pending(request_id);
            self.abort_session(connection, object_server, &session_path)
                .await;
            return Ok((1, HashMap::new()));
        }
        let request = PortalScreenCastRequest {
            request_id,
            deadline_ms: self.timeout.as_millis().min(u32::MAX as u128) as u32,
            parent_surface_id: None,
            app_id: normalized_app_id,
            multiple,
            candidates: Vec::new(),
        };
        if self
            .common
            .send(Event::Portal {
                request: PortalRequest::ScreenCast(request),
                parent_window: plain(parent_window, 128),
            })
            .await
            .is_err()
        {
            self.state.lock().await.cancel_pending(request_id);
            let _ = object_server
                .remove::<RequestObject, _>(handle.as_str())
                .await;
            self.abort_session(connection, object_server, &session_path)
                .await;
            return Ok((1, HashMap::new()));
        }
        let completion = tokio::time::timeout(self.timeout, receiver).await;
        self.state.lock().await.cancel_pending(request_id);
        let _ = object_server
            .remove::<RequestObject, _>(handle.as_str())
            .await;
        let Ok(Ok(completion)) = completion else {
            let _ = self.common.send(Event::PortalCancel(request_id)).await;
            self.abort_session(connection, object_server, &session_path)
                .await;
            return Ok((1, HashMap::new()));
        };
        if completion.reply.decision != PortalDecision::Grant
            || completion.session_id == 0
            || completion.streams.is_empty()
            || completion.streams.len() > if multiple { 4 } else { 1 }
            || completion.reply.surface_ids
                != completion
                    .streams
                    .iter()
                    .map(|stream| stream.surface_id)
                    .collect::<Vec<_>>()
        {
            self.abort_session(connection, object_server, &session_path)
                .await;
            return Ok((1, HashMap::new()));
        }
        let mut streams = Vec::with_capacity(completion.streams.len());
        for (index, stream) in completion.streams.into_iter().enumerate() {
            let mut properties = HashMap::<String, OwnedValue>::new();
            properties.insert("source_type".into(), OwnedValue::from(2u32));
            properties.insert(
                "size".into(),
                owned((i32::from(stream.width), i32::from(stream.height)))?,
            );
            properties.insert(
                "id".into(),
                owned(format!("blit-{}-{index}", completion.session_id))?,
            );
            if stream.pipewire_serial != 0 {
                properties.insert(
                    "pipewire-serial".into(),
                    OwnedValue::from(stream.pipewire_serial),
                );
            }
            streams.push((stream.node_id, properties));
        }
        let mut results = HashMap::new();
        results.insert("streams".into(), owned(streams)?);
        results.insert("persist_mode".into(), OwnedValue::from(0u32));
        Ok((0, results))
    }

    #[zbus(property)]
    fn available_source_types(&self) -> u32 {
        2
    }

    #[zbus(property)]
    fn available_cursor_modes(&self) -> u32 {
        1
    }

    #[zbus(property)]
    fn version(&self) -> u32 {
        6
    }
}

struct SessionObject {
    path: String,
    state: Arc<Mutex<State>>,
    common: Common,
}

#[interface(name = "org.freedesktop.impl.portal.Session")]
impl SessionObject {
    async fn close(&self, #[zbus(object_server)] object_server: &ObjectServer) -> fdo::Result<()> {
        let (session_id, pending_ids) = {
            let mut state = self.state.lock().await;
            let pending_ids = state
                .pending
                .iter()
                .filter_map(|(&request_id, pending)| match pending {
                    Pending::ScreenCast { session_path, .. } if session_path == &self.path => {
                        Some(request_id)
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            for request_id in &pending_ids {
                state.cancel_pending(*request_id);
            }
            let session_id = state
                .sessions
                .remove(&self.path)
                .and_then(|session| session.server_session_id);
            (session_id, pending_ids)
        };
        for request_id in pending_ids {
            let _ = self.common.send(Event::PortalCancel(request_id)).await;
        }
        if let Some(session_id) = session_id {
            let _ = self
                .common
                .send(Event::PortalSessionClosed(session_id))
                .await;
        }
        let _ = object_server
            .remove::<SessionObject, _>(self.path.as_str())
            .await;
        Ok(())
    }

    #[zbus(property)]
    fn version(&self) -> u32 {
        1
    }

    #[zbus(signal)]
    async fn closed(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
}

pub(super) async fn close_server_session(
    connection: &Connection,
    state: &Arc<Mutex<State>>,
    session_id: u32,
) {
    let path = {
        let mut state = state.lock().await;
        let path = state.sessions.iter().find_map(|(path, session)| {
            (session.server_session_id == Some(session_id)).then(|| path.clone())
        });
        if let Some(path) = &path {
            state.sessions.remove(path);
        }
        path
    };
    let Some(path) = path else {
        return;
    };
    if let Ok(emitter) = SignalEmitter::new(connection, path.as_str()) {
        let _ = SessionObject::closed(&emitter).await;
    }
    let _ = connection
        .object_server()
        .remove::<SessionObject, _>(path.as_str())
        .await;
}

fn option_u32_or(
    options: &HashMap<String, OwnedValue>,
    key: &str,
    default: u32,
) -> Result<u32, ()> {
    let Some(value) = options.get(key) else {
        return Ok(default);
    };
    u32::try_from(value).map_err(|_| ())
}

fn option_bool_or(
    options: &HashMap<String, OwnedValue>,
    key: &str,
    default: bool,
) -> Result<bool, ()> {
    let Some(value) = options.get(key) else {
        return Ok(default);
    };
    bool::try_from(value).map_err(|_| ())
}

fn valid_restore_data(options: &HashMap<String, OwnedValue>) -> bool {
    let Some(value) = options.get("restore_data") else {
        return true;
    };
    value
        .try_clone()
        .ok()
        .and_then(|value| <(String, u32, OwnedValue)>::try_from(value).ok())
        .is_some()
}

fn owned<T>(value: T) -> fdo::Result<OwnedValue>
where
    for<'a> zbus::zvariant::Value<'a>: From<T>,
{
    OwnedValue::try_from(zbus::zvariant::Value::from(value))
        .map_err(|error| fdo::Error::Failed(error.to_string()))
}

fn plain(value: &str, max: usize) -> String {
    strip_markup(&clip_text(value, max.min(MAX_STRING_BYTES)), max)
        .chars()
        .filter(|character| !character.is_control())
        .collect()
}

fn plain_body(value: &str, max: usize) -> String {
    strip_markup(&clip_text(value, max.min(MAX_STRING_BYTES)), max)
        .chars()
        .filter(|character| !character.is_control() || matches!(character, '\n' | '\t'))
        .collect()
}

fn normalized_size(request: &PortalAccessRequest) -> usize {
    request.app_id.len()
        + request.title.len()
        + request.subtitle.len()
        + request.body.len()
        + request.deny_label.len()
        + request.grant_label.len()
        + request.icon_name.len()
        + request
            .choices
            .iter()
            .map(|choice| {
                choice.id.len()
                    + choice.label.len()
                    + choice.initial_value.len()
                    + choice
                        .options
                        .iter()
                        .map(|option| option.id.len() + option.value.len())
                        .sum::<usize>()
            })
            .sum::<usize>()
}

type RawChoice = (String, String, Vec<(String, String)>, String);

fn access_choices(options: &HashMap<String, OwnedValue>) -> Result<Vec<PortalChoice>, ()> {
    let Some(value) = options.get("choices") else {
        return Ok(Vec::new());
    };
    let value = value.try_clone().map_err(|_| ())?;
    let raw = Vec::<RawChoice>::try_from(value).map_err(|_| ())?;
    if raw.len() > 16 {
        return Err(());
    }
    let mut choice_ids = HashSet::new();
    let mut normalized = Vec::with_capacity(raw.len());
    for (id, label, raw_options, initial_value) in raw {
        let id = plain(&id, 4 * 1024);
        let label = plain(&label, 4 * 1024);
        let initial_value = plain(&initial_value, 4 * 1024);
        if id.is_empty() || !choice_ids.insert(id.clone()) || raw_options.len() > 32 {
            return Err(());
        }
        let boolean = raw_options.is_empty();
        let mut option_ids = HashSet::new();
        let mut choice_options = Vec::with_capacity(raw_options.len().max(2));
        for (option_id, value) in raw_options {
            let option_id = plain(&option_id, 4 * 1024);
            if option_id.is_empty() || !option_ids.insert(option_id.clone()) {
                return Err(());
            }
            choice_options.push(PortalChoiceValue {
                id: option_id,
                value: plain(&value, 4 * 1024),
            });
        }
        // The portal choice format uses an empty option array for a boolean.
        // Expand it for the viewer while preserving the required returned
        // values, exactly "true" or "false".
        let initial_value = if boolean {
            choice_options = vec![
                PortalChoiceValue {
                    id: "false".into(),
                    value: "false".into(),
                },
                PortalChoiceValue {
                    id: "true".into(),
                    value: "true".into(),
                },
            ];
            match initial_value.as_str() {
                "true" => "true".into(),
                "" | "false" => "false".into(),
                _ => return Err(()),
            }
        } else if initial_value.is_empty() {
            // An empty initial selection delegates the choice to the portal.
            // Pick the first normalized entry deterministically.
            choice_options[0].id.clone()
        } else if choice_options
            .iter()
            .any(|option| option.id == initial_value)
        {
            initial_value
        } else {
            return Err(());
        };
        normalized.push(PortalChoice {
            id,
            label,
            options: choice_options,
            initial_value,
        });
    }
    Ok(normalized)
}

fn validate_choice_reply(
    choices: &[PortalChoice],
    reply: &[PortalChoiceValue],
) -> Option<Vec<(String, String)>> {
    if reply.len() != choices.len() {
        return None;
    }
    choices
        .iter()
        .map(|choice| {
            let value = reply.iter().find(|value| value.id == choice.id)?;
            choice
                .options
                .iter()
                .any(|option| option.id == value.value)
                .then(|| (choice.id.clone(), value.value.clone()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancelling_a_pending_start_does_not_make_the_one_shot_session_retryable() {
        let mut state = State::default();
        let session_path = "/org/freedesktop/portal/desktop/session/test".to_string();
        state.sessions.insert(
            session_path.clone(),
            ScreenCastSession {
                selected: true,
                start_pending: true,
                start_attempted: true,
                ..ScreenCastSession::default()
            },
        );
        let (sender, mut receiver) = oneshot::channel();
        state.pending.insert(
            7,
            Pending::ScreenCast {
                session_path: session_path.clone(),
                sender,
            },
        );

        assert!(state.cancel_pending(7));
        assert!(!state.sessions[&session_path].start_pending);
        assert!(state.sessions[&session_path].start_attempted);
        assert_eq!(
            receiver.try_recv().unwrap().reply.decision,
            PortalDecision::Cancelled
        );
    }

    #[test]
    fn a_late_screencast_completion_is_rejected_but_remains_addressable_for_cleanup() {
        let mut state = State::default();
        let session_path = "/org/freedesktop/portal/desktop/session/test".to_string();
        state.sessions.insert(
            session_path.clone(),
            ScreenCastSession {
                selected: true,
                start_pending: true,
                ..ScreenCastSession::default()
            },
        );
        let (sender, receiver) = oneshot::channel();
        drop(receiver);
        state.pending.insert(
            9,
            Pending::ScreenCast {
                session_path: session_path.clone(),
                sender,
            },
        );

        assert!(matches!(
            state.complete_screencast(9, 11, Vec::new()),
            ScreenCastDelivery::ReceiverDropped
        ));
        assert_eq!(state.sessions[&session_path].server_session_id, Some(11));
    }

    #[test]
    fn choice_replies_must_match_normalized_ids_and_values() {
        let choices = vec![PortalChoice {
            id: "mode".into(),
            label: "Mode".into(),
            options: vec![
                PortalChoiceValue {
                    id: "a".into(),
                    value: "A".into(),
                },
                PortalChoiceValue {
                    id: "b".into(),
                    value: "B".into(),
                },
            ],
            initial_value: "a".into(),
        }];
        assert_eq!(
            validate_choice_reply(
                &choices,
                &[PortalChoiceValue {
                    id: "mode".into(),
                    value: "b".into()
                }]
            ),
            Some(vec![("mode".into(), "b".into())])
        );
        assert!(
            validate_choice_reply(
                &choices,
                &[PortalChoiceValue {
                    id: "mode".into(),
                    value: "injected".into()
                }]
            )
            .is_none()
        );
    }

    #[test]
    fn boolean_and_unspecified_initial_choices_are_normalized() {
        let raw_choices = vec![
            (
                "remember".to_string(),
                "Remember".to_string(),
                Vec::<(String, String)>::new(),
                String::new(),
            ),
            (
                "mode".to_string(),
                "Mode".to_string(),
                vec![
                    ("first".to_string(), "First".to_string()),
                    ("second".to_string(), "Second".to_string()),
                ],
                String::new(),
            ),
        ];
        let mut options = HashMap::new();
        options.insert(
            "choices".into(),
            OwnedValue::try_from(zbus::zvariant::Value::from(raw_choices)).unwrap(),
        );

        let choices = access_choices(&options).unwrap();
        assert_eq!(choices.len(), 2);
        assert_eq!(choices[0].initial_value, "false");
        assert_eq!(
            choices[0]
                .options
                .iter()
                .map(|option| option.id.as_str())
                .collect::<Vec<_>>(),
            vec!["false", "true"]
        );
        assert_eq!(choices[1].initial_value, "first");
    }

    #[test]
    fn malformed_access_choices_reject_the_entire_option() {
        let duplicate_choices = vec![
            (
                "mode".to_string(),
                "Mode".to_string(),
                vec![("first".to_string(), "First".to_string())],
                "first".to_string(),
            ),
            (
                "mode".to_string(),
                "Duplicate".to_string(),
                vec![("second".to_string(), "Second".to_string())],
                "second".to_string(),
            ),
        ];
        let mut options = HashMap::new();
        options.insert(
            "choices".into(),
            OwnedValue::try_from(zbus::zvariant::Value::from(duplicate_choices)).unwrap(),
        );
        assert!(access_choices(&options).is_err());

        options.insert("choices".into(), OwnedValue::from(true));
        assert!(access_choices(&options).is_err());
    }

    #[test]
    fn restore_data_must_have_the_backend_suv_structure() {
        let mut options = HashMap::new();
        options.insert(
            "restore_data".into(),
            OwnedValue::try_from(zbus::zvariant::Value::from((
                "blit",
                1u32,
                zbus::zvariant::Value::from("opaque"),
            )))
            .unwrap(),
        );
        assert!(valid_restore_data(&options));

        options.insert("restore_data".into(), OwnedValue::from(1u32));
        assert!(!valid_restore_data(&options));
    }

    #[test]
    fn session_object_pressure_evicts_only_the_oldest_prestart_session() {
        let mut state = State::default();
        for index in 0..MAX_SESSION_OBJECTS {
            state
                .insert_session(format!("/session/{index}"), "org.example.App".into())
                .unwrap();
        }
        state.sessions.get_mut("/session/0").unwrap().start_pending = true;
        state.sessions.get_mut("/session/1").unwrap().selected = true;

        let evicted = state
            .insert_session("/session/new".into(), "org.example.New".into())
            .unwrap();
        assert_eq!(evicted.as_deref(), Some("/session/1"));
        assert!(state.sessions.contains_key("/session/0"));
        assert!(state.sessions.contains_key("/session/new"));
        assert_eq!(state.sessions.len(), MAX_SESSION_OBJECTS);

        for session in state.sessions.values_mut() {
            session.start_pending = true;
        }
        assert!(
            state
                .insert_session("/session/rejected".into(), "org.example.App".into())
                .is_err()
        );
        assert_eq!(state.sessions.len(), MAX_SESSION_OBJECTS);
    }
}
