use std::{
    collections::VecDeque,
    sync::atomic::{AtomicU64, Ordering},
};

use blit_guest::native_host;

static SEEN_EXTENSION: AtomicU64 = AtomicU64::new(0);

fn extension(client: blit_guest::Client) -> Result<(), ()> {
    SEEN_EXTENSION.store(client.context().extension_id, Ordering::Relaxed);
    Ok(())
}

blit_guest::entry!(extension);

struct Host {
    incoming: VecDeque<Vec<u8>>,
}

impl native_host::Host for Host {
    fn send(&mut self, _: &[u8]) -> i32 {
        0
    }

    fn recv(&mut self, buffer: &mut [u8]) -> i32 {
        let Some(packet) = self.incoming.front() else {
            return 0;
        };
        let len = packet.len();
        if len <= buffer.len() {
            buffer[..len].copy_from_slice(packet);
            self.incoming.pop_front();
        }
        len as i32
    }

    fn wait(&mut self, _: i64) -> i32 {
        if self.incoming.is_empty() { 2 } else { 1 }
    }

    fn clock(&mut self, _: i32) -> i64 {
        0
    }

    fn random(&mut self, destination: &mut [u8]) {
        destination.fill(4);
    }
}

fn hello() -> Vec<u8> {
    let mut packet = vec![0x07];
    packet.extend_from_slice(&1u16.to_le_bytes());
    packet.extend_from_slice(&(1u32 << 11).to_le_bytes());
    packet
}

fn init() -> Vec<u8> {
    let mut packet = vec![0x92, 1];
    packet.extend_from_slice(&99u64.to_le_bytes());
    packet.extend_from_slice(&1u64.to_le_bytes());
    packet.extend_from_slice(&1u64.to_le_bytes());
    packet.extend_from_slice(&1u32.to_le_bytes());
    packet.push(0b0100);
    packet.extend_from_slice(&[0; 32]);
    packet.extend_from_slice(&0u16.to_le_bytes());
    packet.extend_from_slice(&0u16.to_le_bytes());
    packet
}

#[test]
fn exported_entry_bootstraps_before_user_code() {
    let host = Host {
        incoming: [hello(), vec![0x09], init()].into(),
    };
    let _guard = native_host::install(host);

    assert_eq!(__blit_guest_main(), 0);
    assert_eq!(SEEN_EXTENSION.load(Ordering::Relaxed), 99);
}
