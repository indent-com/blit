//! Drop retransmissions of the peer's final DTLS handshake flight.
//!
//! # Why this exists
//!
//! str0m's DTLS backend (`dimpl`) keeps inbound datagrams in a receive queue
//! and frees them from the *front* only, in sequence order
//! (`purge_handled_queue_rx`).  A record that is never marked handled therefore
//! pins the head of that queue and stops it draining at all.
//!
//! A ChangeCipherSpec is exactly such a record.  `dimpl` consumes the first CCS
//! in `await_change_cipher_spec` and sweeps whatever CCS copies are queued at
//! that instant (`drop_pending_ccs`), but once the handshake state machine has
//! moved on nothing ever looks for a CCS again.  A CCS that arrives *after*
//! that point is queued and never handled.
//!
//! DTLS retransmits the final flight routinely — on loss, on a flight timeout,
//! or when the peer sees a duplicate of our own flight — and str0m coalesces it
//! as `[ChangeCipherSpec][Finished]` in a single datagram.  So the late CCS is
//! not an edge case: in practice it arrives on essentially every session.
//!
//! The result is that the receive queue stops draining, every subsequent record
//! accumulates behind the pinned CCS, and at `max_queue_rx` (30) `dimpl` fails
//! the whole session with `ReceiveQueueFull`.  Sessions died after ~29 inbound
//! records — a handful of seconds of use, or one large response — which made
//! every `blit … --on share:…` invocation pay a fresh ~3.5 s ICE+DTLS setup and
//! truncated any transfer bigger than a few packets.
//!
//! # What we do
//!
//! WebRTC never renegotiates DTLS, so a session sees exactly one genuine
//! ChangeCipherSpec per direction.  We let the first one through and drop any
//! later datagram that leads with an epoch-0 CCS, which is by definition a
//! retransmission of a flight we have already processed in full (the datagram
//! carries the duplicate `Finished` with it, and UDP delivers a datagram whole
//! or not at all, so nothing else is lost with it).
//!
//! Gating on "handshake complete" instead would be too late: the retransmission
//! typically arrives *before* the DataChannel opens.
//!
//! Remove this once `dimpl` discards stale epoch-0 ChangeCipherSpec records the
//! way it already discards stale epoch-0 handshake records.

/// DTLS `ContentType::ChangeCipherSpec`.
const DTLS_CHANGE_CIPHER_SPEC: u8 = 20;

/// Bytes in a DTLS record header: type(1) version(2) epoch(2) sequence(6) length(2).
const DTLS_RECORD_HEADER: usize = 13;

/// Per-session filter for duplicate DTLS ChangeCipherSpec flights.
///
/// One instance per `Rtc`, shared by every inbound path (host sockets and the
/// TURN relay all feed the same DTLS engine, so the state must be shared).
#[derive(Default)]
pub struct DtlsFlightDedupe {
    seen_change_cipher_spec: bool,
}

impl DtlsFlightDedupe {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns `true` if `datagram` should be handed to str0m.
    ///
    /// Only ever rejects a repeated epoch-0 ChangeCipherSpec; everything else —
    /// STUN, RTP, and every other DTLS record — passes through untouched.
    pub fn accept(&mut self, datagram: &[u8]) -> bool {
        if datagram.len() < DTLS_RECORD_HEADER || datagram[0] != DTLS_CHANGE_CIPHER_SPEC {
            return true;
        }
        // Content types 20..=63 are DTLS; STUN starts 0x00/0x01 and RTP 0x80+,
        // so a leading byte of 20 is unambiguously a ChangeCipherSpec record.
        let epoch = u16::from_be_bytes([datagram[3], datagram[4]]);
        if epoch != 0 {
            return true;
        }
        if self.seen_change_cipher_spec {
            verbose!("dropping retransmitted DTLS ChangeCipherSpec flight");
            return false;
        }
        self.seen_change_cipher_spec = true;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a DTLS record header followed by `payload`.
    fn record(content_type: u8, epoch: u16, seq: u64, payload: &[u8]) -> Vec<u8> {
        let mut v = vec![content_type, 0xfe, 0xfd];
        v.extend_from_slice(&epoch.to_be_bytes());
        v.extend_from_slice(&seq.to_be_bytes()[2..]); // uint48
        v.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        v.extend_from_slice(payload);
        v
    }

    /// The real shape: `[CCS][Finished]` coalesced into one datagram.
    fn final_flight(ccs_seq: u64, finished_seq: u64) -> Vec<u8> {
        let mut v = record(DTLS_CHANGE_CIPHER_SPEC, 0, ccs_seq, &[1]);
        v.extend_from_slice(&record(22, 1, finished_seq, &[0u8; 48]));
        v
    }

    #[test]
    fn first_flight_passes_and_retransmissions_are_dropped() {
        let mut d = DtlsFlightDedupe::new();
        assert!(d.accept(&final_flight(10, 0)), "first flight must pass");
        assert!(!d.accept(&final_flight(11, 1)), "retransmission must drop");
        assert!(!d.accept(&final_flight(12, 2)), "and stay dropped");
    }

    #[test]
    fn application_data_always_passes() {
        let mut d = DtlsFlightDedupe::new();
        assert!(d.accept(&final_flight(10, 0)));
        for seq in 0..64 {
            assert!(d.accept(&record(23, 1, seq, &[0u8; 64])));
        }
    }

    #[test]
    fn handshake_and_alert_records_pass() {
        let mut d = DtlsFlightDedupe::new();
        assert!(d.accept(&record(22, 0, 0, &[0u8; 70])));
        assert!(d.accept(&record(22, 0, 1, &[0u8; 70])), "flight resends pass");
        assert!(d.accept(&record(21, 1, 0, &[0u8; 2])));
    }

    #[test]
    fn non_dtls_traffic_passes() {
        let mut d = DtlsFlightDedupe::new();
        // STUN binding request.
        assert!(d.accept(&[0x00, 0x01, 0x00, 0x00, 0x21, 0x12, 0xa4, 0x42]));
        // RTP.
        assert!(d.accept(&[0x80, 0x60, 0x00, 0x01, 0, 0, 0, 0, 0, 0, 0, 0, 0]));
        // Truncated / empty.
        assert!(d.accept(&[]));
        assert!(d.accept(&[DTLS_CHANGE_CIPHER_SPEC]));
    }

    /// A CCS at a non-zero epoch is not the handshake flight; leave it alone.
    #[test]
    fn encrypted_epoch_ccs_passes() {
        let mut d = DtlsFlightDedupe::new();
        assert!(d.accept(&record(DTLS_CHANGE_CIPHER_SPEC, 0, 10, &[1])));
        assert!(d.accept(&record(DTLS_CHANGE_CIPHER_SPEC, 1, 11, &[1])));
    }
}
