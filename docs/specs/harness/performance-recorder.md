# Performance recorder

Borrowed from [google/perfetto](https://github.com/google/perfetto) (Apache-2.0): the trace format and the
idea of a time-ordered slice/counter stream, not the C++ SDK (see "Why no SDK" below).

## Product rules

- Settings › Performance has a **Record** button and a duration of 10, 20 or 30 seconds (default 20).
  Recording is off by default and costs nothing while off: no timer runs, no allocation happens, and the
  instrumentation points are a single relaxed atomic load each.
- Pressing **Record** starts a fresh trace and switches the button to **Stop**. The trace ends on its
  own after the chosen duration, or when the user presses **Stop**. The result goes to
  `<data dir>/traces/<local date>-<HHMMSS>.perfetto-trace` (`<data dir>` is the same directory the rest of
  the app state lives in, `~/.harness` by default, `general.dataDir` when relocated).
- The page shows, while recording, the elapsed seconds and the number of recorded slices, and afterwards
  the trace path with **Open in Perfetto** and **Show in folder**.
- **Open in Perfetto** opens `https://ui.perfetto.dev` in the default browser. The trace is not uploaded:
  the user drags the file onto the page or uses its *Open trace file* button. Perfetto accepts `file://`
  only from its own picker, so an embedded viewer would have to serve the file over HTTP from the app; that
  is out of scope for this item (plan 10.5 allows either). **Show in folder** reveals the saved file.
- One recording at a time. Starting a recording while one runs is a no-op.
- A trace contains timing, thread ids, span names and message sizes only — never prompt text, tool
  arguments, environment values or API keys. Span names are fixed strings from the instrumentation
  points below.

## Trace format

A Perfetto protobuf `Trace`: a sequence of `TracePacket` messages (`trace.proto`). `crates/harness-perf`
encodes it by hand in ~250 lines of wire-format code, so a trace can be written with no C++ toolchain, no
`protoc` and no generated code, and the release build stays dependency-free.

Packets used:

| Packet | Purpose |
| --- | --- |
| `ClockSnapshot` (6) + `timestamp_clock_id` (58) + `sequence_flags` (13) + `first_packet_on_sequence` (87) | Declares the sequence-local clock 64 (ns since the recording started, absolute) and maps it to `BUILTIN_CLOCK_REALTIME` taken at record start. Every later packet carries `timestamp_clock_id = 64`. |
| `TracePacketDefaults` (59) → `TrackEventDefaults.track_uuid` | Default track of the sequence (the UI thread's track). |
| `TrackDescriptor` (60) with `ProcessDescriptor` (3) / `ThreadDescriptor` (4) / `parent_uuid` (5) | One process track (`Harness`, pid), one thread track per recording thread, one track per counter. |
| `TrackEvent` (11) with `type` (9) `TYPE_SLICE_BEGIN`/`TYPE_SLICE_END`/`TYPE_INSTANT`, `track_uuid` (11), `name` (23 or interned `name_iid` 10), `debug_annotations` (4) | Spans, instants and their fields. |
| `TrackEvent.track_uuid` on a counter track + `extra_counter_values` (12) | Counters (frame duration ms, tokens per second). |
| `InternedData` (12) → `event_names` (2), `debug_annotation_names` (3), `debug_annotation_string_values` (29) | Names are interned once per recording on the first packet that needs them. |

Timestamps are nanoseconds from `std::time::Instant` elapsed since record start, so the trace needs no
platform clock FFI and cannot jump when the wall clock is adjusted.

## Recording model

`crates/harness-perf`:

- `State` holds a `Vec<Event>` behind a `Mutex` plus the start `Instant`. One global recorder per process
  (`OnceLock`), because there is exactly one trace at a time.
- Instrumentation calls (`span`, `instant`, `counter`) take the mutex only while recording. While off they
  read one `AtomicBool` on the fast path and return a guard that does nothing.
- Events are buffered in memory and encoded at `stop`, in timestamp order. A 30 s recording of the app's
  own instrumentation is well under a megabyte, so a disk-backed writer would only add failure modes
  (partial packets, flush ordering). The buffer is capped at 200 000 events; recording stops early and
  reports that, so a runaway instrumentation point cannot exhaust memory.
- Thread tracks are assigned on first use per OS thread id, so slices from the Slint UI thread, the tokio
  runtime and the bridge writer land on separate tracks in Perfetto.

## Instrumentation points

| Span / counter | Where | Track |
| --- | --- | --- |
| `frame` span (open on `BeforeRendering`, closed on `AfterRendering`) | `apps/harness-slint/src/perf/mod.rs`, via `Window::set_rendering_notifier` | UI thread |
| `frame_time` counter, ms with 0.01 resolution | same place, after each frame | counter track |
| `bridge_send` / `bridge_recv` spans, annotated with the byte length of the line only | `crates/harness-agent-hub/src/acp.rs`, one call site per direction | thread that wrote / read the line |
| `tokens_per_second` counter, fed by the existing rate estimate and the reported token count | `apps/harness-slint/src/speed.rs` | counter track |

Instrumentation must never change behaviour: a panicking or failing recorder is caught at the call site,
and the recorder never blocks on IO while a span is open.

## Why no SDK

`perfetto-sdk-sys` builds the Perfetto C++ SDK with its own toolchain (clang, `gn`-style build files,
`build.rs` compiling tens of megabytes of C++); `tracing-perfetto-sdk` wraps it and pulls the same weight
into every build. Plan 10.2 asked for a feature-gated `perfetto-sdk`; a hand-written encoder is a better
trade for a desktop app that records 30 s traces a few times a month: no native build step, no extra
dependency in the release build, and the same `perfetto-trace` output. `tracing-perfetto-file` (pure Rust,
no C++) is the reference for the wire format; its schema constants are the source of the field numbers
above. It is not used directly because it is a `tracing-subscriber` layer and Harness only wants explicit
instrumentation points, not every `tracing` event.

## Acceptance

1. Settings › Performance shows the page with duration 20 s, Record enabled, no trace yet.
2. Record → the button reads Stop, elapsed counts up, and a trace file appears within the chosen duration.
3. `python -c` decoding the file reports a `Trace` whose first packet is a `ClockSnapshot` and whose
   packets include at least one `frame` slice with a begin and an end.
4. Open in Perfetto opens the browser; Show in folder reveals the file in Explorer.
5. With recording off, starting the app and streaming a reply allocates no recorder memory and the frame
   path is unchanged (one atomic load per hook).
6. The trace file contains no prompt text: instrumented annotations are restricted to fixed kinds, counts
   and byte lengths.
