use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub(crate) const PROCESS_TRACK: u64 = 1;
pub(crate) const FRAME_TIME_TRACK: u64 = 2;
pub(crate) const TOKENS_TRACK: u64 = 3;
const FIRST_THREAD_TRACK: u64 = 8;
pub(crate) const MAX_EVENTS: usize = 200_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Counter {
    FrameMilliseconds,
    TokensPerSecond,
}

impl Counter {
    pub(crate) const ALL: [Self; 2] = [Self::FrameMilliseconds, Self::TokensPerSecond];

    pub(crate) const fn track(self) -> u64 {
        match self {
            Self::FrameMilliseconds => FRAME_TIME_TRACK,
            Self::TokensPerSecond => TOKENS_TRACK,
        }
    }

    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::FrameMilliseconds => "frame_time",
            Self::TokensPerSecond => "tokens_per_second",
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) enum Value {
    Int(i64),
    Float(f64),
    Text(&'static str),
}

#[derive(Clone, Debug)]
pub(crate) enum Kind {
    Begin {
        track: u64,
        name: &'static str,
        annotations: Vec<(&'static str, Value)>,
    },
    End {
        track: u64,
    },
    Instant {
        track: u64,
        name: &'static str,
    },
    CounterInt {
        track: u64,
        value: i64,
    },
    CounterFloat {
        track: u64,
        value: f64,
    },
}

#[derive(Clone, Debug)]
pub(crate) struct Event {
    pub(crate) ts: u64,
    pub(crate) kind: Kind,
}

#[derive(Clone, Debug)]
pub(crate) struct Thread {
    pub(crate) track: u64,
    pub(crate) tid: u64,
    pub(crate) name: String,
}

pub(crate) struct State {
    pub(crate) active: bool,
    pub(crate) start: Instant,
    pub(crate) start_realtime_ns: u64,
    pub(crate) events: Vec<Event>,
    pub(crate) threads: Vec<Thread>,
    tracks: HashMap<u64, u64>,
    pub(crate) overflowed: bool,
}

impl State {
    pub(crate) fn new() -> Self {
        Self {
            active: false,
            start: Instant::now(),
            start_realtime_ns: 0,
            events: Vec::new(),
            threads: Vec::new(),
            tracks: HashMap::new(),
            overflowed: false,
        }
    }

    pub(crate) fn begin(&mut self, name: &'static str, annotations: Vec<(&'static str, Value)>) {
        let track = self.current_track();
        self.push(|ts| Event {
            ts,
            kind: Kind::Begin {
                track,
                name,
                annotations,
            },
        });
    }

    pub(crate) fn end(&mut self) {
        let track = self.current_track();
        self.push(|ts| Event {
            ts,
            kind: Kind::End { track },
        });
    }

    pub(crate) fn instant(&mut self, name: &'static str) {
        let track = self.current_track();
        self.push(|ts| Event {
            ts,
            kind: Kind::Instant { track, name },
        });
    }

    #[allow(clippy::needless_pass_by_value)]
    pub(crate) fn counter(&mut self, counter: Counter, value: Value) {
        let track = counter.track();
        self.push(|ts| Event {
            ts,
            kind: match value {
                Value::Int(value) => Kind::CounterInt { track, value },
                Value::Float(value) => Kind::CounterFloat { track, value },
                Value::Text(_) => Kind::CounterInt { track, value: 0 },
            },
        });
    }

    fn push(&mut self, build: impl FnOnce(u64) -> Event) {
        if !self.active || self.overflowed {
            return;
        }
        if self.events.len() >= MAX_EVENTS {
            self.overflowed = true;
            return;
        }
        let ts = u64::try_from(self.start.elapsed().as_nanos()).unwrap_or(u64::MAX);
        let event = build(ts);
        self.events.push(event);
    }

    fn current_track(&mut self) -> u64 {
        let tid = thread_identity::id();
        if let Some(track) = self.tracks.get(&tid) {
            return *track;
        }
        let track = FIRST_THREAD_TRACK + u64::try_from(self.tracks.len()).unwrap_or(0);
        let name = thread_identity::name();
        self.tracks.insert(tid, track);
        self.threads.push(Thread { track, tid, name });
        track
    }

    pub(crate) fn begin_recording(&mut self, now: Instant) {
        self.active = true;
        self.start = now;
        self.start_realtime_ns = realtime_ns();
        self.events.clear();
        self.threads.clear();
        self.tracks.clear();
        self.overflowed = false;
    }

    pub(crate) fn elapsed(&self) -> Duration {
        self.start.elapsed()
    }
}

fn realtime_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| u64::try_from(since.as_nanos()).unwrap_or(0))
}

mod thread_identity {
    use std::cell::{Cell, RefCell};
    use std::sync::atomic::{AtomicU64, Ordering};

    thread_local! {
        static ID: Cell<u64> = const { Cell::new(0) };
        static NAME: RefCell<Option<String>> = const { RefCell::new(None) };
    }

    static NEXT: AtomicU64 = AtomicU64::new(1);

    pub(super) fn id() -> u64 {
        ID.with(|slot| {
            let current = slot.get();
            if current != 0 {
                return current;
            }
            let assigned = NEXT.fetch_add(1, Ordering::Relaxed);
            slot.set(assigned);
            assigned
        })
    }

    pub(super) fn name() -> String {
        NAME.with(|slot| {
            let mut slot = slot.borrow_mut();
            slot.get_or_insert_with(|| {
                std::thread::current()
                    .name()
                    .map_or_else(|| format!("thread-{}", id()), str::to_owned)
            })
            .clone()
        })
    }
}
