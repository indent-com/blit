use alloc::{string::String, vec::Vec};
use core::{fmt, str};

pub(crate) const S2C_HELLO: u8 = 0x07;
pub(crate) const S2C_READY: u8 = 0x09;
pub(crate) const S2C_FRAGMENT: u8 = 0x2b;
pub(crate) const S2C_AUDIO_FRAME: u8 = 0x30;
pub(crate) const EXT_INFO: u8 = 0x92;
pub(crate) const EXT_INFO_INIT: u8 = 1;
pub(crate) const FEATURE_EXTENSION: u32 = 1 << 11;

const PROTOCOL_VERSION: u16 = 1;
const MAX_NAME: usize = 255;
const MAX_ARGS: usize = 1024;
const MAX_ARG: usize = 64 * 1024;
const MAX_ARGUMENT_BYTES: usize = 1024 * 1024;
const VALID_INIT_FLAGS: u8 = 0b1111;

/// Server identity and protocol capabilities from `S2C_HELLO`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hello {
    pub protocol_version: u16,
    pub features: u32,
    pub boot_generation: Option<u64>,
    pub server_version: Option<String>,
}

/// Immutable identity supplied to one extension attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Context {
    pub hello: Hello,
    pub extension_id: u64,
    pub definition_revision: u64,
    pub attempt: u64,
    pub task_id: u32,
    pub module_hash: [u8; 32],
    pub name: Option<String>,
    pub args: Vec<String>,
    pub detached: bool,
    pub persistent: bool,
    pub enabled: bool,
    pub desired_running: bool,
}

/// Invalid private bootstrap data or ordering.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    EndpointClosed,
    ExpectedHello,
    UnsupportedProtocol(u16),
    ExtensionFeatureMissing,
    InvalidHello,
    DuplicateHello,
    InitBeforeReady,
    ExpectedInit,
    InvalidInit,
    InvalidUtf8,
    InvalidIdentity,
    InvalidFlags(u8),
    InvalidName,
    InvalidArguments,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EndpointClosed => f.write_str("endpoint closed during extension bootstrap"),
            Self::ExpectedHello => f.write_str("extension bootstrap did not begin with HELLO"),
            Self::UnsupportedProtocol(version) => {
                write!(f, "unsupported Blit protocol version {version}")
            }
            Self::ExtensionFeatureMissing => {
                f.write_str("HELLO did not advertise extension support")
            }
            Self::InvalidHello => f.write_str("malformed HELLO packet"),
            Self::DuplicateHello => f.write_str("duplicate HELLO during extension bootstrap"),
            Self::InitBeforeReady => f.write_str("extension INIT arrived before READY"),
            Self::ExpectedInit => f.write_str("first packet after READY was not extension INIT"),
            Self::InvalidInit => f.write_str("malformed extension INIT"),
            Self::InvalidUtf8 => f.write_str("bootstrap string is not UTF-8"),
            Self::InvalidIdentity => f.write_str("extension INIT contains a zero identity field"),
            Self::InvalidFlags(flags) => write!(f, "extension INIT has reserved flags {flags:#x}"),
            Self::InvalidName => f.write_str("extension INIT name is invalid"),
            Self::InvalidArguments => f.write_str("extension INIT arguments exceed their bounds"),
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl std::error::Error for Error {}

pub(crate) fn parse_hello(packet: &[u8]) -> Result<Hello, Error> {
    if packet.first() != Some(&S2C_HELLO) {
        return Err(Error::ExpectedHello);
    }
    if packet.len() < 7 {
        return Err(Error::InvalidHello);
    }
    let protocol_version = read_u16(packet, 1).ok_or(Error::InvalidHello)?;
    if protocol_version != PROTOCOL_VERSION {
        return Err(Error::UnsupportedProtocol(protocol_version));
    }
    let features = read_u32(packet, 3).ok_or(Error::InvalidHello)?;
    if features & FEATURE_EXTENSION == 0 {
        return Err(Error::ExtensionFeatureMissing);
    }

    let (boot_generation, server_version) = match packet.len() {
        7 => (None, None),
        15 => (Some(read_u64(packet, 7).ok_or(Error::InvalidHello)?), None),
        len if len >= 17 => {
            let generation = read_u64(packet, 7).ok_or(Error::InvalidHello)?;
            let version_len = read_u16(packet, 15).ok_or(Error::InvalidHello)? as usize;
            let version = packet
                .get(17..17 + version_len)
                .ok_or(Error::InvalidHello)?;
            if 17 + version_len != packet.len() {
                return Err(Error::InvalidHello);
            }
            let version = str::from_utf8(version).map_err(|_| Error::InvalidUtf8)?;
            (
                Some(generation),
                (!version.is_empty()).then(|| String::from(version)),
            )
        }
        _ => return Err(Error::InvalidHello),
    };

    Ok(Hello {
        protocol_version,
        features,
        boot_generation,
        server_version,
    })
}

pub(crate) fn parse_init(packet: &[u8], hello: Hello) -> Result<Context, Error> {
    if packet.first() != Some(&EXT_INFO) || packet.get(1) != Some(&EXT_INFO_INIT) {
        return Err(Error::ExpectedInit);
    }
    let mut decoder = Decoder::new(packet, 2);
    let extension_id = decoder.u64()?;
    let definition_revision = decoder.u64()?;
    let attempt = decoder.u64()?;
    let task_id = decoder.u32()?;
    if extension_id == 0 || definition_revision == 0 || attempt == 0 || task_id == 0 {
        return Err(Error::InvalidIdentity);
    }
    let flags = decoder.u8()?;
    if flags & !VALID_INIT_FLAGS != 0 {
        return Err(Error::InvalidFlags(flags));
    }
    let mut module_hash = [0u8; 32];
    module_hash.copy_from_slice(decoder.take(32)?);

    let name_len = decoder.u16()? as usize;
    if name_len > MAX_NAME {
        return Err(Error::InvalidName);
    }
    let name = str::from_utf8(decoder.take(name_len)?).map_err(|_| Error::InvalidUtf8)?;
    if name.chars().any(char::is_control) {
        return Err(Error::InvalidName);
    }
    let name = (!name.is_empty()).then(|| String::from(name));

    let argc = decoder.u16()? as usize;
    if argc > MAX_ARGS {
        return Err(Error::InvalidArguments);
    }
    let mut args = Vec::with_capacity(argc);
    let mut argument_bytes = 0usize;
    for _ in 0..argc {
        let len = decoder.u32()? as usize;
        argument_bytes = argument_bytes
            .checked_add(len)
            .ok_or(Error::InvalidArguments)?;
        if len > MAX_ARG || argument_bytes > MAX_ARGUMENT_BYTES {
            return Err(Error::InvalidArguments);
        }
        let argument = str::from_utf8(decoder.take(len)?).map_err(|_| Error::InvalidUtf8)?;
        args.push(String::from(argument));
    }
    decoder.finish()?;

    Ok(Context {
        hello,
        extension_id,
        definition_revision,
        attempt,
        task_id,
        module_hash,
        name,
        args,
        detached: flags & 1 != 0,
        persistent: flags & 2 != 0,
        enabled: flags & 4 != 0,
        desired_running: flags & 8 != 0,
    })
}

fn read_u16(packet: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        packet.get(offset..offset + 2)?.try_into().ok()?,
    ))
}

fn read_u32(packet: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        packet.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

fn read_u64(packet: &[u8], offset: usize) -> Option<u64> {
    Some(u64::from_le_bytes(
        packet.get(offset..offset + 8)?.try_into().ok()?,
    ))
}

struct Decoder<'a> {
    packet: &'a [u8],
    offset: usize,
}

impl<'a> Decoder<'a> {
    fn new(packet: &'a [u8], offset: usize) -> Self {
        Self { packet, offset }
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], Error> {
        let end = self.offset.checked_add(len).ok_or(Error::InvalidInit)?;
        let value = self
            .packet
            .get(self.offset..end)
            .ok_or(Error::InvalidInit)?;
        self.offset = end;
        Ok(value)
    }

    fn u8(&mut self) -> Result<u8, Error> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, Error> {
        Ok(u16::from_le_bytes(
            self.take(2)?.try_into().expect("decoder checked length"),
        ))
    }

    fn u32(&mut self) -> Result<u32, Error> {
        Ok(u32::from_le_bytes(
            self.take(4)?.try_into().expect("decoder checked length"),
        ))
    }

    fn u64(&mut self) -> Result<u64, Error> {
        Ok(u64::from_le_bytes(
            self.take(8)?.try_into().expect("decoder checked length"),
        ))
    }

    fn finish(self) -> Result<(), Error> {
        if self.offset == self.packet.len() {
            Ok(())
        } else {
            Err(Error::InvalidInit)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn hello_rejects_trailing_or_missing_extension_feature() {
        let mut hello = vec![S2C_HELLO];
        hello.extend_from_slice(&PROTOCOL_VERSION.to_le_bytes());
        hello.extend_from_slice(&0u32.to_le_bytes());
        assert_eq!(parse_hello(&hello), Err(Error::ExtensionFeatureMissing));

        hello[3..7].copy_from_slice(&FEATURE_EXTENSION.to_le_bytes());
        hello.push(0);
        assert_eq!(parse_hello(&hello), Err(Error::InvalidHello));
    }
}
