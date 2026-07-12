//! Pipeline hot-path benchmarks (workspace benchmark harness — talker TODO,
//! external review 2026-07-11). Three scenarios from the filed list:
//!
//! - per-chunk ingest cost at the typical 64-byte serial read size (the floor:
//!   activity meter + scrollback append, no rules, no recorders);
//! - the same ingest in steady state at the scrollback byte cap, where every
//!   chunk front-evicts (`VecDeque` drain churn — the "(behind benchmarks)"
//!   eviction item);
//! - match-rule scan scaling (1/8/32 `BytePattern` rules that never match —
//!   pure scan cost, the Aho–Corasick candidate's baseline).
//!
//! Run with `cargo bench -p listener`. These are baselines for the perf items
//! filed in the TODOs — measure here before optimizing there.

use criterion::{criterion_group, criterion_main, BatchSize, Criterion};
use listener::config::{MatchCondition, MatchRule};
use listener::core::{ChannelId, ChunkTime};
use listener::runtime::{ChannelPipeline, PipelineCapacities};
use listener::transport::{ReceivedData, ReceivedPayload};

/// A 64-byte NMEA-shaped line: the common serial read-chunk size (§147).
fn payload_64b() -> Vec<u8> {
    let mut line = b"$GPGGA,123519,4807.038,N,01131.000,E,1,08,0.9,545.4,M,46.9,M".to_vec();
    line.extend_from_slice(b",*\r\n");
    assert_eq!(line.len(), 64);
    line
}

fn chunk(cid: ChannelId, bytes: &[u8]) -> ReceivedData {
    ReceivedData {
        channel_id: cid,
        payload: ReceivedPayload::Bytes(bytes.to_vec()),
        received_at: ChunkTime::now(),
    }
}

/// `n` enabled `BytePattern` rules that can never match the bench payload —
/// the per-chunk scan cost with zero firing overhead.
fn no_match_rules(n: usize) -> Vec<MatchRule> {
    (0..n)
        .map(|i| MatchRule {
            name: format!("bench-rule-{i}"),
            condition: MatchCondition::BytePattern {
                pattern: format!("$GPZZZ,{i},NOMATCH*").into_bytes(),
            },
            actions: Vec::new(),
            enabled: true,
        })
        .collect()
}

fn bench_ingest(c: &mut Criterion) {
    let payload = payload_64b();
    let cid = ChannelId::new();

    // TRUE below-cap floor: a fresh, lightly warmed pipeline per iteration
    // (setup is untimed), so no sample ever measures the capped state. The
    // first version reused one pipeline across the whole run — it filled to
    // the cap after ~2k iterations, so "floor" and "at-cap" measured the
    // same thing and the eviction-cost comparison was self-to-self.
    c.bench_function("ingest/64B/below-cap", |b| {
        b.iter_batched(
            || {
                let mut p = ChannelPipeline::new(cid, PipelineCapacities::default());
                for _ in 0..16 {
                    p.ingest(chunk(cid, &payload));
                }
                (p, chunk(cid, &payload))
            },
            |(mut p, data)| p.ingest(data),
            BatchSize::LargeInput,
        )
    });

    // Steady state at the byte cap: pre-fill past `stream_display` so every
    // chunk evicts its own length from the front.
    let mut at_cap = ChannelPipeline::new(cid, PipelineCapacities::default());
    let cap_bytes = PipelineCapacities::default().stream_display;
    for _ in 0..(cap_bytes / payload.len() + 16) {
        at_cap.ingest(chunk(cid, &payload));
    }
    c.bench_function("ingest/64B/at-scrollback-cap", |b| {
        b.iter_batched(
            || chunk(cid, &payload),
            |data| at_cap.ingest(data),
            BatchSize::SmallInput,
        )
    });
}

fn bench_match_rule_scaling(c: &mut Criterion) {
    let payload = payload_64b();
    let cid = ChannelId::new();
    for n in [1usize, 8, 32] {
        let mut pipeline = ChannelPipeline::new(cid, PipelineCapacities::default())
            .with_match_rules(&no_match_rules(n));
        c.bench_function(&format!("ingest/64B/{n}-byte-pattern-rules"), |b| {
            b.iter_batched(
                || chunk(cid, &payload),
                |data| pipeline.ingest(data),
                BatchSize::SmallInput,
            )
        });
    }
}

criterion_group!(benches, bench_ingest, bench_match_rule_scaling);
criterion_main!(benches);
