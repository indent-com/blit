//! Typed `blit.cli.v1` command-provider support.

use alloc::{string::String, vec::Vec};
use core::{fmt, str};

use blit_remote::{
    STATUS_OK,
    channel::{CHANNEL_MAX_PAYLOAD, FEATURE_CHANNEL},
    extension::{
        self as extension_wire, EXT_INFO_COMMAND_REGISTERED, ExtensionInfo, ExtensionMessage,
        FEATURE_EXTENSION,
    },
};

use crate::{
    Client, Error as ClientError,
    channel::{
        Channel, CloseReason, Closed, Error as ChannelError, Event as ChannelEvent, Listener,
        ListenerEvent,
    },
};

const INVOKE: u8 = 1;
const STDIN: u8 = 2;
const STDIN_EOF: u8 = 3;
const CANCEL: u8 = 4;
const INVOKE_STDIN: u8 = 1;

const STDOUT: u8 = 1;
const STDERR: u8 = 2;
const LOG: u8 = 3;
const RESULT: u8 = 4;
const EXIT: u8 = 5;

/// Typed command-provider failure.
#[derive(Debug)]
pub enum Error {
    Client(ClientError),
    Channel(ChannelError),
    Extension(extension_wire::ExtensionDecodeError),
    FeatureMissing,
    InvalidContext,
    InvalidDescriptor,
    Registration { status: u8, detail: String },
    RegistrationIdentity,
    InvalidInvocation(&'static str),
    PayloadTooLarge,
    AllocationFailed,
    InvalidContentType,
    DuplicateResult,
    Finished,
    Closed(Closed),
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Client(error) => write!(formatter, "guest client error: {error}"),
            Self::Channel(error) => write!(formatter, "native-channel error: {error}"),
            Self::Extension(error) => write!(formatter, "extension wire error: {error}"),
            Self::FeatureMissing => formatter
                .write_str("server HELLO must advertise both extension and native-channel support"),
            Self::InvalidContext => formatter
                .write_str("command providers require a named, persistent extension attempt"),
            Self::InvalidDescriptor => {
                formatter.write_str("command descriptor is empty or exceeds 64 KiB")
            }
            Self::Registration { status, detail } => {
                write!(
                    formatter,
                    "command registration failed with status {status}"
                )?;
                if !detail.is_empty() {
                    write!(formatter, ": {detail}")?;
                }
                Ok(())
            }
            Self::RegistrationIdentity => {
                formatter.write_str("command registration named a different extension revision")
            }
            Self::InvalidInvocation(detail) => {
                write!(formatter, "invalid blit.cli.v1 invocation: {detail}")
            }
            Self::PayloadTooLarge => {
                formatter.write_str("blit.cli.v1 payload exceeds the channel limit")
            }
            Self::AllocationFailed => {
                formatter.write_str("could not allocate a blit.cli.v1 payload")
            }
            Self::InvalidContentType => {
                formatter.write_str("result content type is not a canonical lowercase media type")
            }
            Self::DuplicateResult => {
                formatter.write_str("an invocation may send at most one structured result")
            }
            Self::Finished => formatter.write_str("the invocation already sent EXIT"),
            Self::Closed(closed) => {
                write!(
                    formatter,
                    "invocation channel closed with reason {}",
                    closed.reason
                )?;
                if !closed.detail.is_empty() {
                    write!(formatter, ": {}", closed.detail)?;
                }
                Ok(())
            }
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl std::error::Error for Error {}

impl From<ClientError> for Error {
    fn from(value: ClientError) -> Self {
        Self::Client(value)
    }
}

impl From<ChannelError> for Error {
    fn from(value: ChannelError) -> Self {
        Self::Channel(value)
    }
}

impl From<extension_wire::ExtensionDecodeError> for Error {
    fn from(value: extension_wire::ExtensionDecodeError) -> Self {
        Self::Extension(value)
    }
}

/// One advertised command listener and its live registration.
#[derive(Debug)]
pub struct CommandProvider {
    listener: Listener,
}

/// A command-provider listener event.
#[derive(Debug)]
pub enum ProviderEvent {
    Invocation(Invocation),
    Closed(Closed),
}

impl CommandProvider {
    /// Register a live listener and UTF-8 JSON descriptor as this attempt's
    /// `blit.cli.v1` command surface.
    pub fn register(
        client: &mut Client,
        mut listener: Listener,
        descriptor: &str,
    ) -> Result<Self, Error> {
        if let Err(error) = register_descriptor(client, listener.id(), descriptor) {
            let _ = listener.close(client, CloseReason::Cancelled);
            return Err(error);
        }
        Ok(Self { listener })
    }

    pub const fn listener_id(&self) -> u32 {
        self.listener.id()
    }

    pub fn listener_name(&self) -> &str {
        self.listener.name()
    }

    /// Atomically replace the advertised descriptor while retaining the same
    /// listener generation.
    pub fn update_descriptor(
        &mut self,
        client: &mut Client,
        descriptor: &str,
    ) -> Result<(), Error> {
        register_descriptor(client, self.listener.id(), descriptor)
    }

    /// Remove the advertisement without closing the underlying listener.
    pub fn unregister(&mut self, client: &mut Client) -> Result<(), Error> {
        register_descriptor(client, 0, "")
    }

    /// Wait for one token-fenced CLI connection, then require its first DATA
    /// message to be a valid `INVOKE`.
    pub fn accept(&mut self, client: &mut Client) -> Result<ProviderEvent, Error> {
        match self.listener.accept(client)? {
            ListenerEvent::Accepted(channel) => {
                Invocation::begin(client, channel).map(ProviderEvent::Invocation)
            }
            ListenerEvent::Closed(closed) => Ok(ProviderEvent::Closed(closed)),
        }
    }

    /// Unregister and close the listener. The close still runs if unregister
    /// fails, so a stale advertisement cannot keep accepting invocations.
    pub fn close(&mut self, client: &mut Client) -> Result<(), Error> {
        let unregister = self.unregister(client);
        let close = self
            .listener
            .close(client, CloseReason::Normal)
            .map_err(Error::from);
        unregister.and(close)
    }
}

/// The decoded first message on an invocation channel.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InvocationRequest {
    pub args: Vec<String>,
    pub streams_stdin: bool,
}

/// One input event after `INVOKE`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Input {
    Stdin(Vec<u8>),
    StdinEof,
    Cancel,
    Closed(Closed),
}

/// A log severity supported by `blit.cli.v1`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum LogLevel {
    Trace = 0,
    Debug = 1,
    Info = 2,
    Warning = 3,
    Error = 4,
}

/// One accepted command invocation and its channel bookkeeping.
#[derive(Debug)]
pub struct Invocation {
    channel: Channel,
    request: InvocationRequest,
    stdin_done: bool,
    result_sent: bool,
    finished: bool,
}

impl Invocation {
    fn begin(client: &mut Client, mut channel: Channel) -> Result<Self, Error> {
        loop {
            match channel.receive(client)? {
                ChannelEvent::Data(delivery) => {
                    let request = match decode_invocation(delivery.payload()) {
                        Ok(request) => request,
                        Err(error) => {
                            let _ = channel.discard(client, delivery);
                            let _ = channel.close(client, CloseReason::Cancelled);
                            return Err(error);
                        }
                    };
                    channel.discard(client, delivery)?;
                    let stdin_done = !request.streams_stdin;
                    return Ok(Self {
                        channel,
                        request,
                        stdin_done,
                        result_sent: false,
                        finished: false,
                    });
                }
                ChannelEvent::Acknowledged { .. } => {}
                ChannelEvent::Closed(closed) => return Err(Error::Closed(closed)),
            }
        }
    }

    pub const fn channel_id(&self) -> u32 {
        self.channel.id()
    }

    pub fn peer(&self) -> &str {
        self.channel.peer()
    }

    pub fn metadata(&self) -> &[u8] {
        self.channel.metadata()
    }

    pub const fn request(&self) -> &InvocationRequest {
        &self.request
    }

    /// Receive streamed stdin, EOF, cancellation, or channel closure. ACK-only
    /// messages are consumed internally while maintaining send credit.
    pub fn receive_input(&mut self, client: &mut Client) -> Result<Input, Error> {
        if self.finished {
            return Err(Error::Finished);
        }
        loop {
            match self.channel.receive(client)? {
                ChannelEvent::Acknowledged { .. } => {}
                ChannelEvent::Closed(closed) => return Ok(Input::Closed(closed)),
                ChannelEvent::Data(delivery) => {
                    let previous_stdin_done = self.stdin_done;
                    let parsed = self.decode_input(delivery.payload());
                    if let Err(error) = parsed {
                        self.stdin_done = previous_stdin_done;
                        let _ = self.channel.discard(client, delivery);
                        let _ = self.channel.close(client, CloseReason::Cancelled);
                        return Err(error);
                    }
                    if let Err(error) = self.channel.discard(client, delivery) {
                        self.stdin_done = previous_stdin_done;
                        return Err(error.into());
                    }
                    return parsed;
                }
            }
        }
    }

    pub fn stdout(&mut self, client: &mut Client, data: &[u8]) -> Result<(), Error> {
        self.send_output(client, stdout_payload(data)?)
    }

    pub fn stderr(&mut self, client: &mut Client, data: &[u8]) -> Result<(), Error> {
        self.send_output(client, stderr_payload(data)?)
    }

    pub fn log(
        &mut self,
        client: &mut Client,
        level: LogLevel,
        message: &str,
    ) -> Result<(), Error> {
        self.send_output(client, log_payload(level, message)?)
    }

    pub fn result(
        &mut self,
        client: &mut Client,
        content_type: &str,
        data: &[u8],
    ) -> Result<(), Error> {
        if self.result_sent {
            return Err(Error::DuplicateResult);
        }
        self.send_output(client, result_payload(content_type, data)?)?;
        self.result_sent = true;
        Ok(())
    }

    /// Send the required terminal payload, then begin a normal channel close.
    pub fn exit(&mut self, client: &mut Client, code: i32, detail: &str) -> Result<(), Error> {
        if self.finished {
            return Err(Error::Finished);
        }
        let payload = exit_payload(code, detail)?;
        self.channel.send(client, &payload)?;
        self.finished = true;
        self.channel.close(client, CloseReason::Normal)?;
        Ok(())
    }

    pub fn cancel(&mut self, client: &mut Client) -> Result<(), Error> {
        self.finished = true;
        self.channel.close(client, CloseReason::Cancelled)?;
        Ok(())
    }

    fn send_output(&mut self, client: &mut Client, payload: Vec<u8>) -> Result<(), Error> {
        if self.finished {
            return Err(Error::Finished);
        }
        self.channel.send(client, &payload)?;
        Ok(())
    }

    fn decode_input(&mut self, payload: &[u8]) -> Result<Input, Error> {
        let Some((&kind, body)) = payload.split_first() else {
            return Err(Error::InvalidInvocation("empty DATA payload"));
        };
        match kind {
            STDIN if !self.stdin_done && self.request.streams_stdin => {
                Ok(Input::Stdin(body.to_vec()))
            }
            STDIN_EOF if body.is_empty() && !self.stdin_done && self.request.streams_stdin => {
                self.stdin_done = true;
                Ok(Input::StdinEof)
            }
            CANCEL if body.is_empty() => {
                self.stdin_done = true;
                Ok(Input::Cancel)
            }
            STDIN | STDIN_EOF => Err(Error::InvalidInvocation(
                "stdin arrived after EOF or without the stdin flag",
            )),
            CANCEL => Err(Error::InvalidInvocation("CANCEL has a body")),
            _ => Err(Error::InvalidInvocation("unknown input kind")),
        }
    }
}

/// Build one typed standard-output payload.
pub fn stdout_payload(data: &[u8]) -> Result<Vec<u8>, Error> {
    bytes_payload(STDOUT, data)
}

/// Build one typed standard-error payload.
pub fn stderr_payload(data: &[u8]) -> Result<Vec<u8>, Error> {
    bytes_payload(STDERR, data)
}

/// Build one typed log payload.
pub fn log_payload(level: LogLevel, message: &str) -> Result<Vec<u8>, Error> {
    let mut payload = payload_with_capacity(2, message.len())?;
    payload.push(LOG);
    payload.push(level as u8);
    payload.extend_from_slice(message.as_bytes());
    Ok(payload)
}

/// Build one typed structured-result payload.
pub fn result_payload(content_type: &str, data: &[u8]) -> Result<Vec<u8>, Error> {
    if !valid_content_type(content_type) {
        return Err(Error::InvalidContentType);
    }
    let mut payload = payload_with_capacity(3 + content_type.len(), data.len())?;
    payload.push(RESULT);
    payload.extend_from_slice(&(content_type.len() as u16).to_le_bytes());
    payload.extend_from_slice(content_type.as_bytes());
    payload.extend_from_slice(data);
    Ok(payload)
}

/// Build one typed terminal exit payload.
pub fn exit_payload(code: i32, detail: &str) -> Result<Vec<u8>, Error> {
    let mut payload = payload_with_capacity(5, detail.len())?;
    payload.push(EXIT);
    payload.extend_from_slice(&code.to_le_bytes());
    payload.extend_from_slice(detail.as_bytes());
    Ok(payload)
}

fn bytes_payload(kind: u8, data: &[u8]) -> Result<Vec<u8>, Error> {
    let mut payload = payload_with_capacity(1, data.len())?;
    payload.push(kind);
    payload.extend_from_slice(data);
    Ok(payload)
}

fn payload_with_capacity(prefix: usize, body: usize) -> Result<Vec<u8>, Error> {
    let total = prefix.checked_add(body).ok_or(Error::PayloadTooLarge)?;
    if total > CHANNEL_MAX_PAYLOAD {
        return Err(Error::PayloadTooLarge);
    }
    let mut payload = Vec::new();
    payload
        .try_reserve_exact(total)
        .map_err(|_| Error::AllocationFailed)?;
    Ok(payload)
}

fn valid_content_type(content_type: &str) -> bool {
    if content_type.is_empty() || content_type.len() > 255 {
        return false;
    }
    let mut components = content_type.split('/');
    let Some(left) = components.next() else {
        return false;
    };
    let Some(right) = components.next() else {
        return false;
    };
    components.next().is_none() && valid_media_component(left) && valid_media_component(right)
}

fn valid_media_component(component: &str) -> bool {
    let mut bytes = component.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    (first.is_ascii_lowercase() || first.is_ascii_digit())
        && bytes.all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"!#$&^_.+-".contains(&byte)
        })
}

fn decode_invocation(payload: &[u8]) -> Result<InvocationRequest, Error> {
    if payload.len() > CHANNEL_MAX_PAYLOAD || payload.first() != Some(&INVOKE) || payload.len() < 4
    {
        return Err(Error::InvalidInvocation("INVOKE header is malformed"));
    }
    let flags = payload[1];
    if flags & !INVOKE_STDIN != 0 {
        return Err(Error::InvalidInvocation("INVOKE has reserved flags"));
    }
    let count = u16::from_le_bytes([payload[2], payload[3]]) as usize;
    if count > extension_wire::EXT_MAX_ARGS {
        return Err(Error::InvalidInvocation("too many arguments"));
    }
    let mut offset = 4usize;
    let mut argument_bytes = 0usize;
    let mut args = Vec::new();
    args.try_reserve_exact(count)
        .map_err(|_| Error::AllocationFailed)?;
    for _ in 0..count {
        let length_end = offset
            .checked_add(4)
            .ok_or(Error::InvalidInvocation("argument length overflow"))?;
        let length_bytes = payload
            .get(offset..length_end)
            .ok_or(Error::InvalidInvocation("truncated argument length"))?;
        let length = u32::from_le_bytes(
            length_bytes
                .try_into()
                .expect("checked invocation argument length"),
        ) as usize;
        if length > extension_wire::EXT_MAX_ARG {
            return Err(Error::InvalidInvocation("argument is too large"));
        }
        argument_bytes = argument_bytes
            .checked_add(length)
            .ok_or(Error::InvalidInvocation("argument bytes overflow"))?;
        if argument_bytes > extension_wire::EXT_MAX_ARGUMENT_BYTES {
            return Err(Error::InvalidInvocation("argument vector is too large"));
        }
        offset = length_end;
        let argument_end = offset
            .checked_add(length)
            .ok_or(Error::InvalidInvocation("argument length overflow"))?;
        let argument = payload
            .get(offset..argument_end)
            .ok_or(Error::InvalidInvocation("truncated argument"))?;
        let argument = str::from_utf8(argument)
            .map_err(|_| Error::InvalidInvocation("argument is not UTF-8"))?;
        args.push(String::from(argument));
        offset = argument_end;
    }
    if offset != payload.len() {
        return Err(Error::InvalidInvocation("INVOKE has trailing bytes"));
    }
    Ok(InvocationRequest {
        args,
        streams_stdin: flags & INVOKE_STDIN != 0,
    })
}

fn register_descriptor(
    client: &mut Client,
    listener_id: u32,
    descriptor: &str,
) -> Result<(), Error> {
    let features = client.context().hello.features;
    if features & (FEATURE_EXTENSION | FEATURE_CHANNEL) != FEATURE_EXTENSION | FEATURE_CHANNEL {
        return Err(Error::FeatureMissing);
    }
    if !client.context().persistent || client.context().name.is_none() {
        return Err(Error::InvalidContext);
    }
    let nonce = client.allocate_extension_nonce();
    let request = extension_wire::msg_extension_command_register(nonce, listener_id, descriptor)
        .ok_or(Error::InvalidDescriptor)?;
    client.send(&request)?;
    let packet = client
        .recv_matching(|packet| command_registration_packet(packet, nonce))?
        .ok_or(ClientError::EndpointClosed)?;
    let registered = match extension_wire::parse_extension_message(&packet)? {
        Some(ExtensionMessage::Info(ExtensionInfo::CommandRegistered(registered)))
            if registered.nonce == nonce =>
        {
            registered
        }
        _ => return Err(Error::InvalidInvocation("unexpected registration reply")),
    };
    if registered.status != STATUS_OK {
        return Err(Error::Registration {
            status: registered.status,
            detail: String::from(registered.detail),
        });
    }
    if registered.extension_id != client.context().extension_id
        || registered.definition_revision != client.context().definition_revision
    {
        return Err(Error::RegistrationIdentity);
    }
    Ok(())
}

fn command_registration_packet(packet: &[u8], nonce: u16) -> bool {
    if packet.first() != Some(&extension_wire::EXT_INFO)
        || packet.get(1) != Some(&EXT_INFO_COMMAND_REGISTERED)
    {
        return false;
    }
    matches!(
        extension_wire::parse_extension_message(packet),
        Ok(Some(ExtensionMessage::Info(ExtensionInfo::CommandRegistered(registered))))
            if registered.nonce == nonce
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native_host;
    use alloc::{collections::VecDeque, rc::Rc, vec, vec::Vec};
    use core::cell::RefCell;

    use blit_remote::{channel as channel_wire, extension::ExtensionRequest};

    #[derive(Default)]
    struct State {
        incoming: VecDeque<Vec<u8>>,
        sent: Vec<Vec<u8>>,
    }

    struct MockHost(Rc<RefCell<State>>);

    impl native_host::Host for MockHost {
        fn send(&mut self, packet: &[u8]) -> i32 {
            self.0.borrow_mut().sent.push(packet.to_vec());
            0
        }

        fn recv(&mut self, buffer: &mut [u8]) -> i32 {
            let mut state = self.0.borrow_mut();
            let Some(packet) = state.incoming.front() else {
                return 0;
            };
            let len = packet.len();
            if len <= buffer.len() {
                buffer[..len].copy_from_slice(packet);
                state.incoming.pop_front();
            }
            len as i32
        }

        fn wait(&mut self, _: i64) -> i32 {
            if self.0.borrow().incoming.is_empty() {
                2
            } else {
                1
            }
        }

        fn clock(&mut self, _: i32) -> i64 {
            0
        }

        fn random(&mut self, destination: &mut [u8]) {
            destination.fill(5);
        }
    }

    fn hello() -> Vec<u8> {
        let mut packet = vec![0x07];
        packet.extend_from_slice(&1u16.to_le_bytes());
        packet.extend_from_slice(&(FEATURE_EXTENSION | FEATURE_CHANNEL).to_le_bytes());
        packet
    }

    fn init() -> Vec<u8> {
        let mut packet = vec![0x92, 1];
        packet.extend_from_slice(&7u64.to_le_bytes());
        packet.extend_from_slice(&9u64.to_le_bytes());
        packet.extend_from_slice(&11u64.to_le_bytes());
        packet.extend_from_slice(&13u32.to_le_bytes());
        packet.push(0b1110);
        packet.extend_from_slice(&[42; 32]);
        packet.extend_from_slice(&4u16.to_le_bytes());
        packet.extend_from_slice(b"demo");
        packet.extend_from_slice(&0u16.to_le_bytes());
        packet
    }

    fn boot() -> (native_host::Guard, Rc<RefCell<State>>, Client) {
        let state = Rc::new(RefCell::new(State::default()));
        state
            .borrow_mut()
            .incoming
            .extend([hello(), vec![0x09], init()]);
        let guard = native_host::install(MockHost(Rc::clone(&state)));
        let client = Client::bootstrap().expect("valid extension bootstrap");
        (guard, state, client)
    }

    fn registration(nonce: u16) -> Vec<u8> {
        extension_wire::msg_extension_command_registered(
            &extension_wire::ExtensionCommandRegistered {
                nonce,
                status: STATUS_OK,
                extension_id: 7,
                definition_revision: 9,
                detail: "",
            },
        )
        .unwrap()
    }

    fn invoke(streams_stdin: bool, args: &[&str]) -> Vec<u8> {
        let mut payload = vec![INVOKE, u8::from(streams_stdin) * INVOKE_STDIN];
        payload.extend_from_slice(&(args.len() as u16).to_le_bytes());
        for argument in args {
            payload.extend_from_slice(&(argument.len() as u32).to_le_bytes());
            payload.extend_from_slice(argument.as_bytes());
        }
        payload
    }

    #[test]
    fn provider_registers_decodes_invocation_and_builds_typed_output() {
        let (_guard, state, mut client) = boot();
        state
            .borrow_mut()
            .incoming
            .push_back(channel_wire::msg_channel_opened(2, STATUS_OK, 0, "", b"", "").unwrap());
        let listener = client
            .listen_channel("blit.cli.0000000000000007.11", b"")
            .unwrap();

        state.borrow_mut().incoming.push_back(registration(1));
        let descriptor = r#"{"protocol":"blit.cli.v1","summary":"Demo","commands":[]}"#;
        let mut provider = CommandProvider::register(&mut client, listener, descriptor).unwrap();
        assert_eq!(provider.listener_id(), 2);
        assert_eq!(provider.listener_name(), "blit.cli.0000000000000007.11");
        assert!(matches!(
            extension_wire::parse_extension_request(&state.borrow().sent[1]).unwrap(),
            Some(ExtensionRequest::CommandRegister {
                nonce: 1,
                listener_id: 2,
                descriptor: value,
            }) if value == descriptor
        ));

        let invocation_payload = invoke(true, &["build", "--release", "app"]);
        state.borrow_mut().incoming.extend([
            channel_wire::msg_channel_accepted(
                3,
                2,
                channel_wire::CHANNEL_WINDOW_BYTES,
                "client:0000000000000001",
                b"cli",
            )
            .unwrap(),
            channel_wire::msg_channel_data(3, &invocation_payload).unwrap(),
        ]);
        let ProviderEvent::Invocation(mut invocation) = provider.accept(&mut client).unwrap()
        else {
            panic!("expected invocation");
        };
        assert_eq!(invocation.channel_id(), 3);
        assert_eq!(invocation.peer(), "client:0000000000000001");
        assert_eq!(invocation.metadata(), b"cli");
        assert_eq!(
            invocation.request(),
            &InvocationRequest {
                args: ["build", "--release", "app"]
                    .into_iter()
                    .map(String::from)
                    .collect(),
                streams_stdin: true,
            }
        );

        state
            .borrow_mut()
            .incoming
            .push_back(channel_wire::msg_channel_data(3, &[STDIN, b'a', b'b']).unwrap());
        assert_eq!(
            invocation.receive_input(&mut client).unwrap(),
            Input::Stdin(b"ab".to_vec())
        );
        state
            .borrow_mut()
            .incoming
            .push_back(channel_wire::msg_channel_data(3, &[STDIN_EOF]).unwrap());
        assert_eq!(
            invocation.receive_input(&mut client).unwrap(),
            Input::StdinEof
        );
        state
            .borrow_mut()
            .incoming
            .push_back(channel_wire::msg_channel_data(3, &[CANCEL]).unwrap());
        assert_eq!(
            invocation.receive_input(&mut client).unwrap(),
            Input::Cancel
        );

        invocation.stdout(&mut client, b"building\n").unwrap();
        invocation.stderr(&mut client, b"warning\n").unwrap();
        invocation
            .log(&mut client, LogLevel::Info, "halfway")
            .unwrap();
        invocation
            .result(&mut client, "application/json", br#"{"ok":true}"#)
            .unwrap();
        assert!(matches!(
            invocation.result(&mut client, "application/json", b"{}"),
            Err(Error::DuplicateResult)
        ));
        invocation.exit(&mut client, 0, "done").unwrap();
        assert!(matches!(
            invocation.stdout(&mut client, b"late"),
            Err(Error::Finished)
        ));

        let state = state.borrow();
        let data_payloads = state
            .sent
            .iter()
            .filter_map(|packet| match channel_wire::parse_channel_request(packet) {
                Ok(Some(channel_wire::ChannelRequest::Data {
                    channel_id: 3,
                    payload,
                })) => Some(payload.to_vec()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            data_payloads,
            [
                stdout_payload(b"building\n").unwrap(),
                stderr_payload(b"warning\n").unwrap(),
                log_payload(LogLevel::Info, "halfway").unwrap(),
                result_payload("application/json", br#"{"ok":true}"#).unwrap(),
                exit_payload(0, "done").unwrap(),
            ]
        );
        assert!(state.sent.iter().any(|packet| {
            matches!(
                channel_wire::parse_channel_request(packet),
                Ok(Some(channel_wire::ChannelRequest::Close {
                    channel_id: 3,
                    reason: channel_wire::CHANNEL_CLOSE_NORMAL,
                }))
            )
        }));
        let acked = state
            .sent
            .iter()
            .filter_map(|packet| match channel_wire::parse_channel_request(packet) {
                Ok(Some(channel_wire::ChannelRequest::Ack {
                    channel_id: 3,
                    bytes,
                })) => Some(bytes),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            acked,
            [
                invocation_payload.len() as u64,
                (invocation_payload.len() + 3) as u64,
                (invocation_payload.len() + 4) as u64,
                (invocation_payload.len() + 5) as u64,
            ]
        );
    }

    #[test]
    fn malformed_invocations_and_result_types_are_rejected() {
        assert!(matches!(
            decode_invocation(&[INVOKE, 0x80, 0, 0]),
            Err(Error::InvalidInvocation("INVOKE has reserved flags"))
        ));
        assert!(matches!(
            decode_invocation(&[INVOKE, 0, 1, 0, 3, 0, 0, 0, 0xff, 0xff, 0xff]),
            Err(Error::InvalidInvocation("argument is not UTF-8"))
        ));
        assert!(result_payload("application/json", b"{}").is_ok());
        assert!(result_payload("application/octet-stream", b"").is_ok());
        for invalid in [
            "Application/json",
            "application",
            "application/*",
            "application/json; charset=utf-8",
            "/json",
            "application/",
            "a/b/c",
        ] {
            assert!(matches!(
                result_payload(invalid, b""),
                Err(Error::InvalidContentType)
            ));
        }
    }
}
