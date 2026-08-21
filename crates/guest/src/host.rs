//! Safe wrappers around the five `blit_v1` host imports.

use core::fmt;

/// Largest complete packet accepted by `blit_v1.send` or returned by
/// `blit_v1.recv`.
pub const MAX_PACKET_SIZE: usize = 16 * 1024 * 1024;
/// Largest single `blit_v1.random` request.
pub const MAX_RANDOM_CHUNK: usize = 64 * 1024;

/// A successful low-level send outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SendOutcome {
    /// The complete packet was accepted.
    Accepted,
    /// The endpoint closed before accepting the packet.
    Closed,
    /// The host rejected the packet's size.
    RejectedSize,
}

/// A low-level receive outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecvOutcome {
    /// The endpoint is closed and its mailbox is empty.
    Closed,
    /// The next packet was copied into the start of the supplied buffer.
    Copied(usize),
    /// The next packet remains queued and needs this capacity.
    NeedsCapacity(usize),
}

/// Which clock is read by [`clock`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(i32)]
pub enum ClockKind {
    /// Signed nanoseconds since the Unix epoch.
    Realtime = 0,
    /// Signed nanoseconds from an unspecified monotonic origin.
    Monotonic = 1,
}

/// Result of parking in [`wait`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitOutcome {
    /// The absolute monotonic deadline was reached.
    Deadline,
    /// A subsequent receive can make progress.
    Packet,
    /// The endpoint is closed and its mailbox is empty.
    Closed,
}

/// A safe-wrapper failure. Invalid pointer ranges never reach the host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    EmptyPacket,
    PacketTooLarge { len: usize },
    AddressOutOfRange,
    InvalidSendResult(i32),
    InvalidRecvResult(i32),
    InvalidWaitResult(i32),
    EntropyUnavailable,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyPacket => f.write_str("a Blit packet cannot be empty"),
            Self::PacketTooLarge { len } => {
                write!(f, "packet length {len} exceeds the 16 MiB host limit")
            }
            Self::AddressOutOfRange => f.write_str("guest memory range does not fit the host ABI"),
            Self::InvalidSendResult(value) => write!(f, "invalid blit_v1.send result {value}"),
            Self::InvalidRecvResult(value) => write!(f, "invalid blit_v1.recv result {value}"),
            Self::InvalidWaitResult(value) => write!(f, "invalid blit_v1.wait result {value}"),
            Self::EntropyUnavailable => f.write_str("host entropy source is unavailable"),
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl std::error::Error for Error {}

/// Send one complete Blit packet.
pub fn send(packet: &[u8]) -> Result<SendOutcome, Error> {
    if packet.is_empty() {
        return Err(Error::EmptyPacket);
    }
    if packet.len() > MAX_PACKET_SIZE {
        return Err(Error::PacketTooLarge { len: packet.len() });
    }
    validate_range(packet.as_ptr(), packet.len())?;
    let result = raw::send(packet);
    match result {
        0 => Ok(SendOutcome::Accepted),
        -1 => Ok(SendOutcome::Closed),
        -2 => Ok(SendOutcome::RejectedSize),
        value => Err(Error::InvalidSendResult(value)),
    }
}

/// Receive or inspect the next complete host packet.
///
/// A [`RecvOutcome::NeedsCapacity`] result leaves the same packet queued.
pub fn recv(buffer: &mut [u8]) -> Result<RecvOutcome, Error> {
    if buffer.len() > MAX_PACKET_SIZE {
        return Err(Error::PacketTooLarge { len: buffer.len() });
    }
    validate_range(buffer.as_ptr(), buffer.len())?;
    let result = raw::recv(buffer);
    if result < 0 {
        return Err(Error::InvalidRecvResult(result));
    }
    let len = result as usize;
    if len == 0 {
        Ok(RecvOutcome::Closed)
    } else if len > MAX_PACKET_SIZE {
        Err(Error::PacketTooLarge { len })
    } else if len > buffer.len() {
        Ok(RecvOutcome::NeedsCapacity(len))
    } else {
        Ok(RecvOutcome::Copied(len))
    }
}

/// Park until a packet, endpoint closure, or an absolute monotonic deadline.
pub fn wait(monotonic_deadline_ns: i64) -> Result<WaitOutcome, Error> {
    match raw::wait(monotonic_deadline_ns) {
        0 => Ok(WaitOutcome::Deadline),
        1 => Ok(WaitOutcome::Packet),
        2 => Ok(WaitOutcome::Closed),
        value => Err(Error::InvalidWaitResult(value)),
    }
}

/// Read a host clock directly, without packet dispatch.
pub fn clock(kind: ClockKind) -> i64 {
    raw::clock(kind as i32)
}

/// Fill an arbitrary-length destination from the host entropy source.
///
/// The host import has a 64 KiB limit, so this wrapper chunks larger fills.
pub fn random(destination: &mut [u8]) -> Result<(), Error> {
    for chunk in destination.chunks_mut(MAX_RANDOM_CHUNK) {
        validate_range(chunk.as_ptr(), chunk.len())?;
        if !raw::random(chunk) {
            return Err(Error::EntropyUnavailable);
        }
    }
    Ok(())
}

#[cfg(target_arch = "wasm32")]
fn validate_range(pointer: *const u8, len: usize) -> Result<(), Error> {
    let start = pointer as usize;
    let end = start.checked_add(len).ok_or(Error::AddressOutOfRange)?;
    if start > i32::MAX as usize || end > i32::MAX as usize || len > i32::MAX as usize {
        return Err(Error::AddressOutOfRange);
    }
    Ok(())
}

#[cfg(not(target_arch = "wasm32"))]
fn validate_range(_pointer: *const u8, _len: usize) -> Result<(), Error> {
    Ok(())
}

#[cfg(target_arch = "wasm32")]
mod raw {
    #[link(wasm_import_module = "blit_v1")]
    unsafe extern "C" {
        #[link_name = "send"]
        fn import_send(pointer: i32, len: i32) -> i32;
        #[link_name = "recv"]
        fn import_recv(pointer: i32, capacity: i32) -> i32;
        #[link_name = "wait"]
        fn import_wait(monotonic_deadline_ns: i64) -> i32;
        #[link_name = "clock"]
        fn import_clock(kind: i32) -> i64;
        #[link_name = "random"]
        fn import_random(pointer: i32, len: i32);
    }

    pub(super) fn send(packet: &[u8]) -> i32 {
        // The public wrapper checked the entire range against signed i32.
        unsafe { import_send(packet.as_ptr() as i32, packet.len() as i32) }
    }

    pub(super) fn recv(buffer: &mut [u8]) -> i32 {
        // The public wrapper checked the entire range against signed i32.
        unsafe { import_recv(buffer.as_mut_ptr() as i32, buffer.len() as i32) }
    }

    pub(super) fn wait(deadline: i64) -> i32 {
        unsafe { import_wait(deadline) }
    }

    pub(super) fn clock(kind: i32) -> i64 {
        unsafe { import_clock(kind) }
    }

    pub(super) fn random(destination: &mut [u8]) -> bool {
        // The public wrapper checked the entire range against signed i32.
        unsafe { import_random(destination.as_mut_ptr() as i32, destination.len() as i32) }
        true
    }
}

#[cfg(not(target_arch = "wasm32"))]
mod raw {
    pub(super) fn send(packet: &[u8]) -> i32 {
        crate::native_host::with(|host| host.send(packet))
    }

    pub(super) fn recv(buffer: &mut [u8]) -> i32 {
        crate::native_host::with(|host| host.recv(buffer))
    }

    pub(super) fn wait(deadline: i64) -> i32 {
        crate::native_host::with(|host| host.wait(deadline))
    }

    pub(super) fn clock(kind: i32) -> i64 {
        crate::native_host::with(|host| host.clock(kind))
    }

    pub(super) fn random(destination: &mut [u8]) -> bool {
        crate::native_host::with(|host| host.try_random(destination))
    }
}
