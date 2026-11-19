use std::sync::Mutex;
use std::time::Duration;

use super::{counter, counter_float, instant, span, span_with, AnnotatedValue, Counter};

static SERIAL: Mutex<()> = Mutex::new(());

fn decode(bytes: &[u8]) -> Vec<(u32, Vec<u8>)> {
    let mut fields = Vec::new();
    let mut cursor = 0;
    while cursor < bytes.len() {
        let (key, read) = varint(&bytes[cursor..]);
        cursor += read;
        let field = u32::try_from(key >> 3).unwrap();
        let wire = u8::try_from(key & 7).unwrap();
        match wire {
            0 => {
                let (value, read) = varint(&bytes[cursor..]);
                cursor += read;
                fields.push((field, value.to_le_bytes().to_vec()));
            }
            1 => {
                fields.push((field, bytes[cursor..cursor + 8].to_vec()));
                cursor += 8;
            }
            2 => {
                let (length, read) = varint(&bytes[cursor..]);
                cursor += read;
                let length = usize::try_from(length).unwrap();
                fields.push((field, bytes[cursor..cursor + length].to_vec()));
                cursor += length;
            }
            other => panic!("unsupported wire type {other}"),
        }
    }
    fields
}

fn varint(bytes: &[u8]) -> (u64, usize) {
    let mut value = 0_u64;
    let mut shift = 0;
    for (index, byte) in bytes.iter().enumerate() {
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return (value, index + 1);
        }
        shift += 7;
    }
    panic!("truncated varint");
}

fn field(fields: &[(u32, Vec<u8>)], number: u32) -> Option<&[u8]> {
    fields
        .iter()
        .find(|(field, _)| *field == number)
        .map(|(_, value)| value.as_slice())
}

fn record(work: impl FnOnce()) -> Vec<u8> {
    let directory = std::env::temp_dir().join("harness-perf-tests");
    super::start().unwrap();
    work();
    let written = super::stop(&directory).unwrap();
    let bytes = std::fs::read(&written.path).unwrap();
    let _ = std::fs::remove_file(&written.path);
    assert!(written.slices >= 1 || written.bytes > 0);
    assert!(written.duration >= Duration::ZERO);
    bytes
}

#[test]
fn trace_starts_with_a_clock_snapshot_and_has_slices() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let bytes = record(|| {
        span("frame").end();
    });
    let packets = decode(&bytes);
    assert_eq!(packets[0].0, 1);
    let first_packet = decode(&packets[0].1);
    assert!(field(&first_packet, 6).is_some());
    assert_eq!(
        field(&first_packet, 87),
        Some(1_u64.to_le_bytes().as_slice())
    );

    let track_events: Vec<Vec<(u32, Vec<u8>)>> = packets
        .iter()
        .filter(|(number, _)| *number == 1)
        .map(|(_, payload)| decode(payload))
        .filter_map(|packet| field(&packet, 11).map(<[u8]>::to_vec))
        .map(|payload| decode(&payload))
        .collect();
    assert_eq!(track_events.len(), 2);
    assert_eq!(
        field(&track_events[0], 9),
        Some(1_u64.to_le_bytes().as_slice())
    );
    assert_eq!(
        field(&track_events[1], 9),
        Some(2_u64.to_le_bytes().as_slice())
    );
}

#[test]
fn annotations_counters_and_instants_are_all_recorded() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let bytes = record(|| {
        span_with(
            "bridge_send",
            vec![
                ("bytes", AnnotatedValue::Int(42)),
                ("kind", AnnotatedValue::Text("chat")),
            ],
        )
        .end();
        instant("ui_event");
        counter(Counter::TokensPerSecond, 37);
        counter_float(Counter::FrameMilliseconds, 16.75);
    });
    let packets: Vec<Vec<(u32, Vec<u8>)>> = decode(&bytes)
        .into_iter()
        .filter(|(number, _)| *number == 1)
        .map(|(_, payload)| decode(&payload))
        .collect();
    let events: Vec<Vec<(u32, Vec<u8>)>> = packets
        .iter()
        .filter_map(|packet| field(packet, 11).map(<[u8]>::to_vec))
        .map(|payload| decode(&payload))
        .collect();
    assert_eq!(events.len(), 5);
    assert!(field(&events[0], 4).is_some());
    assert_eq!(field(&events[2], 9), Some(3_u64.to_le_bytes().as_slice()));
    assert!(field(&events[3], 31).is_some());
    assert!(field(&events[4], 45).is_some());
}

#[test]
fn recorder_is_inert_while_stopped() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert!(!super::is_recording());
    span("frame").end();
    instant("ui_event");
    counter(Counter::TokensPerSecond, 1);
    assert_eq!(super::slice_count(), 0);
    assert!(super::stop(std::env::temp_dir().as_path()).is_err());
}

#[test]
fn starting_twice_is_rejected() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    super::start().unwrap();
    assert!(matches!(
        super::start(),
        Err(super::PerfError::AlreadyRecording)
    ));
    let _ = super::stop(std::env::temp_dir().as_path()).unwrap();
}

#[test]
fn file_name_is_a_local_date_stamp() {
    let name = super::trace_file_name();
    assert!(name.ends_with(".perfetto-trace"), "{name}");
    assert_eq!(
        name.len(),
        "2026-01-31-235959.perfetto-trace".len(),
        "{name}"
    );
}
