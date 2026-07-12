//! Scheduler hot-path benchmarks (workspace benchmark harness — TODO,
//! external review 2026-07-11). The runner's loop calls these once per pass,
//! so their cost scales directly with send rate (ADR-017 moved the practical
//! ceiling to ~500 Hz–1 kHz; these baselines say what the loop can afford):
//!
//! - `poll` when nothing is due — the linear next-fire scan (spec §8.1's
//!   "conceptually a priority queue"; adopt a real heap only if this shows
//!   message-count scans matter);
//! - `poll` returning a due send — includes the per-send wire-bytes clone the
//!   "observer-path allocations" TODO targets (`render_into` candidate);
//! - `min_active_interval` — re-scanned every loop pass for the ADR-017
//!   high-resolution timer gate (caching candidate).
//!
//! Run with `cargo bench -p talker`.

use std::hint::black_box;
use std::time::{Duration, Instant};

use criterion::{criterion_group, criterion_main, Criterion};
use talker::core::message::{
    ChecksumAlgorithm, ChecksumConfig, MessageConfig, PayloadConfig, TimestampConfig,
};
use talker::core::scheduler::{Schedule, Tick};

fn msg(byte_len: usize, interval_ms: u64) -> MessageConfig {
    MessageConfig::new(PayloadConfig::raw_hex("AB".repeat(byte_len)), interval_ms)
}

/// Drain every immediately-due fire so subsequent polls at `now` hit the
/// nothing-due scan path.
fn drain_due(schedule: &mut Schedule, now: Instant) {
    while matches!(schedule.poll(now), Tick::Send { .. }) {}
}

fn bench_poll_scan(c: &mut Criterion) {
    for n in [8usize, 64, 512] {
        let start = Instant::now();
        let messages: Vec<MessageConfig> = (0..n).map(|_| msg(32, 1_000)).collect();
        let mut schedule = Schedule::compile(&messages, start).unwrap();
        drain_due(&mut schedule, start);
        c.bench_function(&format!("schedule/poll-idle-scan/{n}-messages"), |b| {
            b.iter(|| black_box(schedule.poll(black_box(start))))
        });
    }
}

fn bench_poll_due_send(c: &mut Criterion) {
    for (label, bytes) in [("64B", 64usize), ("1KiB", 1024)] {
        let start = Instant::now();
        let mut schedule = Schedule::compile(&[msg(bytes, 1)], start).unwrap();
        let mut now = start;
        c.bench_function(&format!("schedule/poll-due-send/{label}"), |b| {
            b.iter(|| {
                // March exactly one interval per iteration so every poll is a
                // due fire: measures the scan + the per-send payload clone.
                now += Duration::from_millis(1);
                black_box(schedule.poll(now))
            })
        });
    }
}

/// The dynamic-render counterpart of `poll-due-send`: the same 64-byte payload
/// with a full timestamp (date+millis+timezone, three chrono format calls into
/// a temporary `String`) prepended and a CRC-16/CCITT appended per send. The
/// static case's `render_into` KILL verdict covered only the plain payload
/// clone; this is the case that says whether that verdict generalizes to the
/// per-send rendering path (the "observer-path allocations" TODO).
// The config structs are `#[non_exhaustive]`, so a bench (outside the crate)
// cannot use struct literals — Default + field assignment is the only way in.
#[allow(clippy::field_reassign_with_default)]
fn bench_poll_due_send_rendered(c: &mut Criterion) {
    let start = Instant::now();
    let mut message = msg(64, 1);
    let mut ts = TimestampConfig::default();
    ts.include_date = true;
    ts.include_millis = true;
    ts.include_timezone = true;
    message.timestamp = Some(ts);
    let mut cs = ChecksumConfig::default();
    cs.algorithm = ChecksumAlgorithm::Crc16Ccitt;
    message.checksum = Some(cs);
    let mut schedule = Schedule::compile(&[message], start).unwrap();
    let mut now = start;
    c.bench_function("schedule/poll-due-send/64B-timestamp-crc16", |b| {
        b.iter(|| {
            now += Duration::from_millis(1);
            black_box(schedule.poll(now))
        })
    });
}

fn bench_min_active_interval(c: &mut Criterion) {
    let start = Instant::now();
    let messages: Vec<MessageConfig> = (0..512).map(|_| msg(32, 1_000)).collect();
    let schedule = Schedule::compile(&messages, start).unwrap();
    c.bench_function("schedule/min-active-interval/512-messages", |b| {
        b.iter(|| black_box(schedule.min_active_interval()))
    });
}

criterion_group!(
    benches,
    bench_poll_scan,
    bench_poll_due_send,
    bench_poll_due_send_rendered,
    bench_min_active_interval
);
criterion_main!(benches);
