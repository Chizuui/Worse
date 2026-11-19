use std::collections::{BTreeMap, HashMap};

use crate::model::{Counter, Event, Kind, Thread, Value, PROCESS_TRACK};
use crate::proto::Buf;

const CUSTOM_CLOCK: u64 = 64;
const BUILTIN_CLOCK_REALTIME: u64 = 1;
const SEQUENCE_ID: u64 = 1;
const SEQUENCE_FLAGS: u64 = 3;
const TYPE_SLICE_BEGIN: u64 = 1;
const TYPE_SLICE_END: u64 = 2;
const TYPE_INSTANT: u64 = 3;

#[derive(Default)]
struct Interner {
    next: u64,
    ids: HashMap<&'static str, u64>,
    names: BTreeMap<u64, &'static str>,
}

impl Interner {
    fn intern(&mut self, name: &'static str) -> u64 {
        if let Some(id) = self.ids.get(name) {
            return *id;
        }
        self.next += 1;
        self.ids.insert(name, self.next);
        self.names.insert(self.next, name);
        self.next
    }

    fn id(&self, name: &str) -> u64 {
        self.ids.get(name).copied().unwrap_or(0)
    }
}

pub(crate) struct Encoded {
    pub(crate) bytes: Vec<u8>,
    pub(crate) slices: usize,
}

pub(crate) fn encode(
    events: &[Event],
    threads: &[Thread],
    start_realtime_ns: u64,
    process_id: u64,
) -> Encoded {
    let mut interner = Interner::default();
    let mut slices = 0;
    for event in events {
        match &event.kind {
            Kind::Begin {
                name, annotations, ..
            } => {
                slices += 1;
                interner.intern(name);
                for (key, value) in annotations {
                    interner.intern(key);
                    if let Value::Text(text) = value {
                        interner.intern(text);
                    }
                }
            }
            Kind::Instant { name, .. } => {
                slices += 1;
                interner.intern(name);
            }
            Kind::End { .. } | Kind::CounterInt { .. } | Kind::CounterFloat { .. } => {}
        }
    }

    let mut packets = Vec::with_capacity(events.len() + threads.len() + 16);
    packets.push(clock_and_defaults_packet(start_realtime_ns));
    packets.extend(descriptor_packets(process_id, threads));
    packets.push(interned_packet(&interner));
    for event in events {
        packets.push(event_packet(event, &interner));
    }

    let mut bytes = Vec::with_capacity(packets.len() * 48);
    for packet in packets {
        let mut outer = Buf::new();
        outer.message_field(1, packet);
        bytes.extend_from_slice(&outer.into_vec());
    }
    Encoded { bytes, slices }
}

fn shell() -> Buf {
    let mut packet = Buf::new();
    packet.varint_field(10, SEQUENCE_ID);
    packet.varint_field(58, CUSTOM_CLOCK);
    packet
}

fn clock_and_defaults_packet(start_realtime_ns: u64) -> Buf {
    let mut packet = Buf::new();
    packet.varint_field(10, SEQUENCE_ID);
    packet.varint_field(13, SEQUENCE_FLAGS);
    packet.varint_field(87, 1);
    packet.varint_field(58, CUSTOM_CLOCK);

    let mut snapshot = Buf::new();
    let mut realtime = Buf::new();
    realtime.varint_field(1, BUILTIN_CLOCK_REALTIME);
    realtime.varint_field(2, start_realtime_ns);
    snapshot.message_field(1, realtime);
    let mut custom = Buf::new();
    custom.varint_field(1, CUSTOM_CLOCK);
    custom.varint_field(2, 0);
    snapshot.message_field(1, custom);
    snapshot.varint_field(2, BUILTIN_CLOCK_REALTIME);
    packet.message_field(6, snapshot);

    let mut defaults = Buf::new();
    let mut track_defaults = Buf::new();
    track_defaults.varint_field(11, PROCESS_TRACK);
    defaults.message_field(11, track_defaults);
    defaults.varint_field(58, CUSTOM_CLOCK);
    packet.message_field(59, defaults);
    packet
}

fn descriptor_packets(process_id: u64, threads: &[Thread]) -> Vec<Buf> {
    let mut packet = shell();
    let mut descriptor = Buf::new();
    descriptor.varint_field(1, PROCESS_TRACK);
    descriptor.string_field(2, "Harness");
    let mut process = Buf::new();
    process.varint_field(1, process_id);
    process.string_field(6, "Harness");
    descriptor.message_field(3, process);
    packet.message_field(60, descriptor);

    let mut packets = vec![packet];
    for thread in threads {
        let mut packet = shell();
        let mut descriptor = Buf::new();
        descriptor.varint_field(1, thread.track);
        descriptor.string_field(2, &thread.name);
        descriptor.varint_field(5, PROCESS_TRACK);
        let mut thread_descriptor = Buf::new();
        thread_descriptor.varint_field(1, process_id);
        thread_descriptor.varint_field(2, thread.tid);
        thread_descriptor.string_field(5, &thread.name);
        descriptor.message_field(4, thread_descriptor);
        packet.message_field(60, descriptor);
        packets.push(packet);
    }
    for counter in Counter::ALL {
        let mut packet = shell();
        let mut descriptor = Buf::new();
        descriptor.varint_field(1, counter.track());
        descriptor.string_field(2, counter.name());
        descriptor.varint_field(5, PROCESS_TRACK);
        descriptor.message_field(8, Buf::new());
        packet.message_field(60, descriptor);
        packets.push(packet);
    }
    packets
}

fn interned_packet(interner: &Interner) -> Buf {
    let mut data = Buf::new();
    for (id, name) in &interner.names {
        data.message_field(2, interned_entry(*id, name));
        data.message_field(3, interned_entry(*id, name));
        data.message_field(29, interned_entry(*id, name));
    }
    let mut packet = shell();
    packet.message_field(12, data);
    packet
}

fn interned_entry(id: u64, name: &str) -> Buf {
    let mut entry = Buf::new();
    entry.varint_field(1, id);
    entry.string_field(2, name);
    entry
}

fn event_packet(event: &Event, interner: &Interner) -> Buf {
    let mut packet = shell();
    packet.varint_field(8, event.ts);
    let mut track_event = Buf::new();
    match &event.kind {
        Kind::Begin {
            track,
            name,
            annotations,
        } => {
            track_event.varint_field(9, TYPE_SLICE_BEGIN);
            track_event.varint_field(11, *track);
            track_event.varint_field(10, interner.id(name));
            for (key, value) in annotations {
                let mut annotation = Buf::new();
                annotation.varint_field(1, interner.id(key));
                match value {
                    Value::Int(value) => {
                        annotation.varint_field(4, u64::from_ne_bytes(value.to_ne_bytes()));
                    }
                    Value::Float(value) => annotation.double_field(5, *value),
                    Value::Text(text) => annotation.varint_field(17, interner.id(text)),
                }
                track_event.message_field(4, annotation);
            }
        }
        Kind::End { track } => {
            track_event.varint_field(9, TYPE_SLICE_END);
            track_event.varint_field(11, *track);
        }
        Kind::Instant { track, name } => {
            track_event.varint_field(9, TYPE_INSTANT);
            track_event.varint_field(11, *track);
            track_event.varint_field(10, interner.id(name));
        }
        Kind::CounterInt { track, value } => {
            track_event.varint_field(31, *track);
            track_event.varint_field(12, u64::from_ne_bytes(value.to_ne_bytes()));
        }
        Kind::CounterFloat { track, value } => {
            track_event.varint_field(45, *track);
            track_event.double_field(46, *value);
        }
    }
    packet.message_field(11, track_event);
    packet
}
