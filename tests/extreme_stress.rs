
//! EXTREME STRESS TEST — Push every component to absolute limits
//!
//! Run: cargo test --test extreme_stress --release -- --nocapture
//!
//! Performance thresholds only enforced in release mode.
//! Debug mode runs for correctness only.

use std::collections::{HashMap, HashSet};
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use lru::LruCache;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot, Mutex, Semaphore};

/// Performance assertions only fire in release mode
macro_rules! assert_perf {
    ($cond:expr, $($arg:tt)*) => {
        if cfg!(not(debug_assertions)) {
            assert!($cond, $($arg)*);
        } else {
            // In debug mode, just log the result
            if !$cond {
                eprintln!("  [DEBUG MODE] Perf threshold skipped: {}", format!($($arg)*));
            }
        }
    };
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Req {
    jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<i64>,
    method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Resp {
    jsonrpc: String,
    id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<serde_json::Value>,
}

fn make_request(id: i64, method: &str, payload_size: usize) -> Req {
    let params = if payload_size > 0 {
        Some(serde_json::json!({
            "uri": format!("file:///workspace/project/src/file_{}.rs", id),
            "payload": "x".repeat(payload_size),
        }))
    } else {
        Some(serde_json::json!({"uri": format!("file:///w/p/s/f_{}.rs", id)}))
    };
    Req {
        jsonrpc: "2.0".to_string(),
        id: Some(id),
        method: method.to_string(),
        params,
    }
}

// ══════════════════════════════════════════════════════
//  TEST 1: 1M JSON-RPC serialization round-trip
// ══════════════════════════════════════════════════════

#[test]
fn extreme_1m_serialization_roundtrip() {
    const N: usize = 1_000_000;
    let start = Instant::now();
    let mut total_bytes = 0usize;

    for i in 0..N {
        let req = make_request(i as i64, "tools/call", 0);
        let json = serde_json::to_string(&req).unwrap();
        total_bytes += json.len();
        let _parsed: Req = serde_json::from_str(&json).unwrap();
    }

    let elapsed = start.elapsed();
    let rate = N as f64 / elapsed.as_secs_f64();
    let mb = total_bytes as f64 / 1_048_576.0;

    println!("=== 1M Serialization Round-trip ===");
    println!("  Ops:        {}", N);
    println!("  Total data: {:.1} MB", mb);
    println!("  Time:       {:.0}ms", elapsed.as_millis());
    println!("  Throughput: {:.0} round-trips/sec", rate);
    println!("  Bandwidth:  {:.0} MB/sec", mb / elapsed.as_secs_f64());

    assert_perf!(rate > 200_000.0, "Too slow: {:.0}/s", rate);
}

// ══════════════════════════════════════════════════════
//  TEST 2: Large payload serialization (64KB per message)
// ══════════════════════════════════════════════════════

#[test]
fn extreme_large_payload_serialization() {
    const N: usize = 10_000;
    const PAYLOAD_SIZE: usize = 65_536;
    let start = Instant::now();
    let mut total_bytes = 0usize;

    for i in 0..N {
        let req = make_request(i as i64, "resources/read", PAYLOAD_SIZE);
        let json = serde_json::to_string(&req).unwrap();
        total_bytes += json.len();
        let parsed: Req = serde_json::from_str(&json).unwrap();
        assert!(parsed.params.is_some());
    }

    let elapsed = start.elapsed();
    let mb = total_bytes as f64 / 1_048_576.0;
    let bw = mb / elapsed.as_secs_f64();

    println!("=== Large Payload (64KB x {}) ===", N);
    println!("  Total data: {:.0} MB", mb);
    println!("  Time:       {:.0}ms", elapsed.as_millis());
    println!("  Bandwidth:  {:.0} MB/sec", bw);

    assert_perf!(bw > 100.0, "Large payload too slow: {:.0} MB/s", bw);
}

// ══════════════════════════════════════════════════════
//  TEST 3: 200 concurrent tasks fighting over one lock
// ══════════════════════════════════════════════════════

#[tokio::test]
async fn extreme_200_tasks_lock_contention() {
    const NUM_TASKS: usize = 200;
    const OPS_PER_TASK: usize = 5_000;

    let map: Arc<Mutex<HashMap<u64, oneshot::Sender<Resp>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let counter = Arc::new(AtomicU64::new(0));
    let completed = Arc::new(AtomicU64::new(0));

    let start = Instant::now();
    let mut handles = Vec::with_capacity(NUM_TASKS);

    for task_id in 0..NUM_TASKS {
        let map = map.clone();
        let counter = counter.clone();
        let completed = completed.clone();

        handles.push(tokio::spawn(async move {
            for _ in 0..OPS_PER_TASK {
                let id = counter.fetch_add(1, Ordering::SeqCst);
                let (tx, rx) = oneshot::channel();

                { map.lock().await.insert(id, tx); }
                tokio::task::yield_now().await;

                {
                    let mut guard = map.lock().await;
                    if let Some(tx) = guard.remove(&id) {
                        let _ = tx.send(Resp {
                            jsonrpc: "2.0".to_string(),
                            id: Some(id as i64),
                            result: Some(serde_json::json!({"task": task_id})),
                            error: None,
                        });
                    }
                }

                match tokio::time::timeout(Duration::from_millis(100), rx).await {
                    Ok(Ok(_)) => { completed.fetch_add(1, Ordering::Relaxed); }
                    Ok(Err(_)) => {}
                    Err(_) => panic!("Deadlock! Task {} stuck on id {}", task_id, id),
                }
            }
        }));
    }

    for h in handles { h.await.unwrap(); }

    let elapsed = start.elapsed();
    let total_ops = NUM_TASKS * OPS_PER_TASK;
    let rate = total_ops as f64 / elapsed.as_secs_f64();
    let remaining = map.lock().await.len();

    println!("=== 200 Tasks x 5000 Ops Lock Contention ===");
    println!("  Total ops:  {}", total_ops);
    println!("  Completed:  {}", completed.load(Ordering::Relaxed));
    println!("  Leaked:     {}", remaining);
    println!("  Time:       {:.0}ms", elapsed.as_millis());
    println!("  Throughput: {:.0} ops/sec", rate);

    assert_eq!(remaining, 0, "Memory leak: {} entries not cleaned up", remaining);
}

// ══════════════════════════════════════════════════════
//  TEST 4: Channel saturation — fast producer, slow consumer
// ══════════════════════════════════════════════════════

#[tokio::test]
async fn extreme_channel_saturation() {
    const CHANNEL_SIZE: usize = 64;
    const NUM_MESSAGES: usize = 500_000;
    const CONSUMER_DELAY_EVERY: usize = 1000;

    let (tx, mut rx) = mpsc::channel::<Vec<u8>>(CHANNEL_SIZE);
    let produced = Arc::new(AtomicU64::new(0));

    let start = Instant::now();

    let produced_clone = produced.clone();
    let producer = tokio::spawn(async move {
        for i in 0..NUM_MESSAGES {
            let msg = format!(
                "{{\"jsonrpc\":\"2.0\",\"id\":{},\"method\":\"tools/call\",\"params\":{{\"data\":\"{}\"}}}}",
                i, "ab".repeat(50)
            );
            tx.send(msg.into_bytes()).await.unwrap();
            produced_clone.fetch_add(1, Ordering::Relaxed);
        }
    });

    let consumer = tokio::spawn(async move {
        let mut count = 0usize;
        let mut total_bytes = 0usize;
        while let Some(msg) = rx.recv().await {
            total_bytes += msg.len();
            count += 1;
            if count % CONSUMER_DELAY_EVERY == 0 {
                tokio::time::sleep(Duration::from_micros(10)).await;
            }
            if count >= NUM_MESSAGES { break; }
        }
        (count, total_bytes)
    });

    producer.await.unwrap();
    let (count, total_bytes) = consumer.await.unwrap();

    let elapsed = start.elapsed();
    let rate = count as f64 / elapsed.as_secs_f64();
    let mb = total_bytes as f64 / 1_048_576.0;

    println!("=== Channel Saturation (Asymmetric) ===");
    println!("  Channel:    {} slots", CHANNEL_SIZE);
    println!("  Produced:   {}", produced.load(Ordering::Relaxed));
    println!("  Consumed:   {} ({:.1} MB)", count, mb);
    println!("  Time:       {:.0}ms", elapsed.as_millis());
    println!("  Throughput: {:.0} msg/sec", rate);

    assert_eq!(count, NUM_MESSAGES);
}

// ══════════════════════════════════════════════════════
//  TEST 5: Routing worst case — overlapping deep prefixes
// ══════════════════════════════════════════════════════

#[test]
fn extreme_routing_deep_prefixes() {
    const NUM_LOOKUPS: usize = 500_000;

    let roots: Vec<PathBuf> = (1..=20)
        .map(|depth| {
            let mut p = PathBuf::from("/workspace");
            for d in 0..depth { p.push(format!("level_{}", d)); }
            p
        })
        .collect();

    let deepest = &roots[roots.len() - 1];
    let start = Instant::now();
    let mut correct = 0usize;

    for i in 0..NUM_LOOKUPS {
        let lookup = deepest.join(format!("file_{}.rs", i));
        let matched = roots.iter()
            .filter(|root| lookup.starts_with(root))
            .max_by_key(|root| root.components().count())
            .cloned();
        if matched.as_ref() == Some(deepest) { correct += 1; }
    }

    let elapsed = start.elapsed();
    let rate = NUM_LOOKUPS as f64 / elapsed.as_secs_f64();

    println!("=== Routing: Overlapping Deep Prefixes ===");
    println!("  {} nested roots, {} lookups", roots.len(), NUM_LOOKUPS);
    println!("  Correct:    {}/{}", correct, NUM_LOOKUPS);
    println!("  Time:       {:.0}ms", elapsed.as_millis());
    println!("  Throughput: {:.0} lookups/sec", rate);

    assert_eq!(correct, NUM_LOOKUPS);
    assert_perf!(rate > 100_000.0, "Routing too slow: {:.0}/sec", rate);
}

// ══════════════════════════════════════════════════════
//  TEST 6: LRU thrashing — zero hit rate (worst case)
// ══════════════════════════════════════════════════════

#[test]
fn extreme_lru_zero_hit_thrashing() {
    const CACHE_SIZE: usize = 3;
    const NUM_ROOTS: usize = 1000;
    const ROUNDS: usize = 100_000;

    let mut cache: LruCache<PathBuf, String> = LruCache::new(NonZeroUsize::new(CACHE_SIZE).unwrap());
    let roots: Vec<PathBuf> = (0..NUM_ROOTS).map(|i| PathBuf::from(format!("/project_{}", i))).collect();

    let mut hits = 0u64;
    let mut misses = 0u64;
    let mut evictions = 0u64;
    let start = Instant::now();

    for round in 0..ROUNDS {
        let root = &roots[round % NUM_ROOTS];
        if cache.get(root).is_some() {
            hits += 1;
        } else {
            misses += 1;
            if cache.len() >= CACHE_SIZE { evictions += 1; }
            cache.put(root.clone(), format!("backend_{}", round));
        }
    }

    let elapsed = start.elapsed();
    let rate = ROUNDS as f64 / elapsed.as_secs_f64();
    let hit_ratio = hits as f64 / ROUNDS as f64;

    println!("=== LRU Thrashing (Zero Hit) ===");
    println!("  Cache: {}, Roots: {}, Rounds: {}", CACHE_SIZE, NUM_ROOTS, ROUNDS);
    println!("  Hits: {} ({:.2}%)", hits, hit_ratio * 100.0);
    println!("  Misses: {}, Evictions: {}", misses, evictions);
    println!("  Time:       {:.0}ms", elapsed.as_millis());
    println!("  Throughput: {:.0} ops/sec", rate);

    assert!(hit_ratio < 0.01, "Expected near-zero hit rate, got {:.4}", hit_ratio);
    assert_perf!(rate > 1_000_000.0, "LRU thrashing too slow: {:.0}/sec", rate);
}

// ══════════════════════════════════════════════════════
//  TEST 7: Git root cache — 100K unique keys + 1M lookups
// ══════════════════════════════════════════════════════

#[test]
fn extreme_git_cache_100k_keys() {
    const NUM_DIRS: usize = 100_000;
    let mut cache: HashMap<PathBuf, Option<PathBuf>> = HashMap::new();
    let start = Instant::now();

    for i in 0..NUM_DIRS {
        let key = PathBuf::from(format!(
            "/workspace/project_{}/src/module_{}/submodule_{}/deep/path",
            i / 10_000, (i / 100) % 100, i % 100
        ));
        cache.insert(key, Some(PathBuf::from(format!("/workspace/project_{}", i / 10_000))));
    }
    let fill_elapsed = start.elapsed();

    let lookup_start = Instant::now();
    let mut hits = 0u64;
    for i in 0..1_000_000 {
        let key = PathBuf::from(format!(
            "/workspace/project_{}/src/module_{}/submodule_{}/deep/path",
            (i % NUM_DIRS) / 10_000, ((i % NUM_DIRS) / 100) % 100, (i % NUM_DIRS) % 100
        ));
        if cache.get(&key).is_some() { hits += 1; }
    }
    let lookup_elapsed = lookup_start.elapsed();
    let lookup_rate = 1_000_000.0 / lookup_elapsed.as_secs_f64();

    println!("=== Git Cache: 100K Keys + 1M Lookups ===");
    println!("  Fill:    {} entries in {:.0}ms", cache.len(), fill_elapsed.as_millis());
    println!("  Lookups: 1M in {:.0}ms ({:.0}/sec)", lookup_elapsed.as_millis(), lookup_rate);
    println!("  Hits:    {}", hits);

    assert_eq!(cache.len(), NUM_DIRS);
    assert_eq!(hits, 1_000_000);
    assert_perf!(lookup_rate > 1_000_000.0, "Cache lookup too slow: {:.0}/sec", lookup_rate);
}

// ══════════════════════════════════════════════════════
//  TEST 8: Crash recovery — 100 cycles x 500 pending
// ══════════════════════════════════════════════════════

#[tokio::test]
async fn extreme_crash_recovery_cycles() {
    const CYCLES: usize = 100;
    const PENDING_PER_CYCLE: usize = 500;

    let total_drained = Arc::new(AtomicU64::new(0));
    let total_received = Arc::new(AtomicU64::new(0));
    let start = Instant::now();

    for cycle in 0..CYCLES {
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Resp>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let mut receivers = Vec::with_capacity(PENDING_PER_CYCLE);

        {
            let mut map = pending.lock().await;
            for i in 0..PENDING_PER_CYCLE {
                let id = (cycle * PENDING_PER_CYCLE + i) as u64;
                let (tx, rx) = oneshot::channel();
                map.insert(id, tx);
                receivers.push(rx);
            }
        }

        {
            let mut map = pending.lock().await;
            let count = map.len();
            for (id, tx) in map.drain() {
                let _ = tx.send(Resp {
                    jsonrpc: "2.0".to_string(),
                    id: Some(id as i64),
                    result: None,
                    error: Some(serde_json::json!({"code": -32001, "message": "crash"})),
                });
            }
            total_drained.fetch_add(count as u64, Ordering::Relaxed);
        }

        for rx in receivers {
            if let Ok(resp) = rx.await {
                assert!(resp.error.is_some());
                total_received.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    let elapsed = start.elapsed();
    let total = CYCLES * PENDING_PER_CYCLE;
    let rate = total as f64 / elapsed.as_secs_f64();

    println!("=== Repeated Crash Recovery ({} cycles) ===", CYCLES);
    println!("  {} pending/cycle x {} cycles = {} total", PENDING_PER_CYCLE, CYCLES, total);
    println!("  Drained:  {}", total_drained.load(Ordering::Relaxed));
    println!("  Received: {}", total_received.load(Ordering::Relaxed));
    println!("  Time:     {:.0}ms", elapsed.as_millis());
    println!("  Rate:     {:.0} drain-and-recover/sec", rate);

    assert_eq!(total_drained.load(Ordering::Relaxed) as usize, total);
    assert_eq!(total_received.load(Ordering::Relaxed) as usize, total);
}

// ══════════════════════════════════════════════════════
//  TEST 9: Semaphore inflight limiter — 500 tasks x 100 ops
// ══════════════════════════════════════════════════════

#[tokio::test]
async fn extreme_semaphore_inflight_limiter() {
    const MAX_INFLIGHT: usize = 10;
    const NUM_TASKS: usize = 500;
    const WORK_PER_TASK: usize = 100;

    let sem = Arc::new(Semaphore::new(MAX_INFLIGHT));
    let active = Arc::new(AtomicU64::new(0));
    let max_observed = Arc::new(AtomicU64::new(0));
    let completed = Arc::new(AtomicU64::new(0));

    let start = Instant::now();
    let mut handles = Vec::with_capacity(NUM_TASKS);

    for _ in 0..NUM_TASKS {
        let sem = sem.clone();
        let active = active.clone();
        let max_observed = max_observed.clone();
        let completed = completed.clone();

        handles.push(tokio::spawn(async move {
            for _ in 0..WORK_PER_TASK {
                let _permit = sem.acquire().await.unwrap();
                let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                max_observed.fetch_max(current, Ordering::SeqCst);
                tokio::task::yield_now().await;
                active.fetch_sub(1, Ordering::SeqCst);
                completed.fetch_add(1, Ordering::Relaxed);
            }
        }));
    }

    for h in handles { h.await.unwrap(); }

    let elapsed = start.elapsed();
    let total = NUM_TASKS * WORK_PER_TASK;
    let rate = total as f64 / elapsed.as_secs_f64();
    let max_concurrent = max_observed.load(Ordering::SeqCst);

    println!("=== Semaphore Inflight Limiter ===");
    println!("  Max permits:    {}", MAX_INFLIGHT);
    println!("  Tasks:          {}", NUM_TASKS);
    println!("  Total ops:      {}", total);
    println!("  Max concurrent: {}", max_concurrent);
    println!("  Time:           {:.0}ms", elapsed.as_millis());
    println!("  Throughput:     {:.0} ops/sec", rate);

    assert!(
        max_concurrent <= MAX_INFLIGHT as u64,
        "Semaphore violated! {} > {}", max_concurrent, MAX_INFLIGHT
    );
    assert_eq!(completed.load(Ordering::Relaxed) as usize, total);
}

// ══════════════════════════════════════════════════════
//  TEST 10: 5-second sustained endurance run
// ══════════════════════════════════════════════════════

#[test]
fn extreme_sustained_endurance() {
    const DURATION_SECS: u64 = 5;
    const BATCH_SIZE: usize = 10_000;

    let deadline = Instant::now() + Duration::from_secs(DURATION_SECS);
    let mut total_ops = 0u64;
    let mut batch_times = Vec::new();

    let roots: Vec<PathBuf> = (0..20)
        .map(|i| PathBuf::from(format!("/workspace/project_{}", i)))
        .collect();

    let mut cache: HashMap<PathBuf, Option<PathBuf>> = HashMap::new();
    let mut throttle_set: HashSet<PathBuf> = HashSet::new();

    while Instant::now() < deadline {
        let batch_start = Instant::now();

        for i in 0..BATCH_SIZE {
            let idx = (total_ops as usize + i) % 20;
            let req = make_request(total_ops as i64 + i as i64, "tools/call", 0);
            let json = serde_json::to_string(&req).unwrap();
            let _parsed: Req = serde_json::from_str(&json).unwrap();

            let lookup = roots[idx].join(format!("src/file_{}.rs", i));
            let _matched = roots.iter()
                .filter(|r| lookup.starts_with(r))
                .max_by_key(|r| r.as_os_str().len());

            let cache_key = roots[idx].join("src");
            if !cache.contains_key(&cache_key) {
                cache.insert(cache_key, Some(roots[idx].clone()));
            }
            throttle_set.insert(lookup);
        }

        throttle_set.clear();

        batch_times.push(batch_start.elapsed());
        total_ops += BATCH_SIZE as u64;
    }

    let avg_batch_ms = batch_times.iter().map(|d| d.as_secs_f64() * 1000.0).sum::<f64>() / batch_times.len() as f64;
    let min_batch_ms = batch_times.iter().map(|d| d.as_secs_f64() * 1000.0).fold(f64::MAX, f64::min);
    let max_batch_ms = batch_times.iter().map(|d| d.as_secs_f64() * 1000.0).fold(0.0f64, f64::max);
    let rate = total_ops as f64 / DURATION_SECS as f64;
    let degradation_ratio = max_batch_ms / avg_batch_ms;

    println!("=== Sustained Endurance ({}s) ===", DURATION_SECS);
    println!("  Total ops:    {}", total_ops);
    println!("  Batches:      {}", batch_times.len());
    println!("  Avg batch:    {:.2}ms", avg_batch_ms);
    println!("  Min batch:    {:.2}ms", min_batch_ms);
    println!("  Max batch:    {:.2}ms", max_batch_ms);
    println!("  Degradation:  {:.1}x (max/avg)", degradation_ratio);
    println!("  Sustained:    {:.0} ops/sec", rate);

    assert!(total_ops > 100_000, "Too few ops in {}s: {}", DURATION_SECS, total_ops);
    assert_perf!(degradation_ratio < 5.0, "Degradation! {:.1}x", degradation_ratio);
}

// ══════════════════════════════════════════════════════
//  TEST 11: Malicious/edge-case JSON fuzzing
// ══════════════════════════════════════════════════════

#[test]
fn extreme_malicious_json_fuzzing() {
    let deep_braces = "{".repeat(100);
    let deep_brackets = "[".repeat(100);
    let huge_method = format!("{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"{}\"}}", "A".repeat(1_000_000));

    let evil_inputs: Vec<&str> = vec![
        "",
        "   ",
        "\n\n\n",
        "\t\t",
        "\u{FEFF}",
        "\u{FEFF}{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"test\"}",
        &deep_braces,
        &deep_brackets,
        &huge_method,
        "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"test\",\"params\":{\"emoji\":\"hello\"}}",
        "{\"jsonrpc\":\"2.0\"}",
        "{\"jsonrpc\":\"2.0\",\"id\":null,\"method\":\"test\"}",
        "{\"id\":1,\"method\":\"test\"}",
        "{\"jsonrpc\":2.0,\"id\":\"abc\",\"method\":123}",
        "{\"jsonrpc\":\"2.0\",\"id\":[],\"method\":\"test\"}",
        "{\"jsonrpc\":\"2.0\",\"id\":-1,\"method\":\"test\"}",
        "{\"jsonrpc\":\"2.0\",\"id\":0,\"method\":\"test\"}",
        "not json at all",
        "42",
        "true",
        "null",
        "[1,2,3]",
    ];

    let mut parse_ok = 0u64;
    let mut parse_err = 0u64;
    let start = Instant::now();

    for input in &evil_inputs {
        let trimmed = input.trim_start_matches('\u{feff}').trim();
        match serde_json::from_str::<Req>(trimmed) {
            Ok(_) => parse_ok += 1,
            Err(_) => parse_err += 1,
        }
    }

    // Fuzz with 100K variations
    for i in 0..100_000u64 {
        let fuzzed = match i % 5 {
            0 => format!("{{\"jsonrpc\":\"2.0\",\"id\":{},\"method\":\"fuzz_{}\"}}", i, i),
            1 => format!("  {{\"jsonrpc\":\"2.0\",\"id\":{},\"method\":\"fuzz\"}} ", i),
            2 => format!("{{\"jsonrpc\":\"2.0\",\"id\":{},\"method\":\"m\"}}\n", i),
            3 => format!("\u{FEFF}{{\"jsonrpc\":\"2.0\",\"id\":{},\"method\":\"m\"}}", i),
            _ => format!("garbage_{}", i),
        };

        let trimmed = fuzzed.trim_start_matches('\u{feff}').trim();
        match serde_json::from_str::<Req>(trimmed) {
            Ok(_) => parse_ok += 1,
            Err(_) => parse_err += 1,
        }
    }

    let elapsed = start.elapsed();

    println!("=== Malicious JSON Fuzzing ===");
    println!("  Static cases: {}", evil_inputs.len());
    println!("  Fuzz iters:   100000");
    println!("  Parse OK:     {}", parse_ok);
    println!("  Parse Err:    {}", parse_err);
    println!("  Time:         {:.0}ms", elapsed.as_millis());
    println!("  NO PANICS — all inputs handled gracefully");

    assert!(parse_ok > 0);
    assert!(parse_err > 0);
}

// ══════════════════════════════════════════════════════
//  TEST 12: Path grouping — 100K paths x 50 roots
// ══════════════════════════════════════════════════════

#[test]
fn extreme_path_grouping_100k() {
    const NUM_ROOTS: usize = 50;
    const NUM_PATHS: usize = 100_000;

    let roots: Vec<PathBuf> = (0..NUM_ROOTS)
        .map(|i| PathBuf::from(format!("/workspace/org_{}/project_{}", i / 10, i)))
        .collect();

    let paths: Vec<PathBuf> = (0..NUM_PATHS)
        .map(|i| {
            let r = i % NUM_ROOTS;
            PathBuf::from(format!("/workspace/org_{}/project_{}/src/deep/file_{}.rs", r / 10, r, i))
        })
        .collect();

    let start = Instant::now();

    let mut groups: HashMap<PathBuf, Vec<String>> = HashMap::new();
    for path in &paths {
        let root = roots.iter()
            .filter(|r| path.starts_with(r))
            .max_by_key(|r| r.as_os_str().len())
            .cloned();
        if let Some(root) = root {
            let uri = format!("file:///{}", path.display().to_string().replace('\\', "/"));
            groups.entry(root).or_default().push(uri);
        }
    }

    let elapsed = start.elapsed();
    let rate = NUM_PATHS as f64 / elapsed.as_secs_f64();
    let total_uris: usize = groups.values().map(|v| v.len()).sum();

    println!("=== Path Grouping: 100K x 50 Roots ===");
    println!("  Groups:     {}", groups.len());
    println!("  Total URIs: {}", total_uris);
    println!("  Time:       {:.0}ms", elapsed.as_millis());
    println!("  Throughput: {:.0} paths/sec", rate);

    assert_eq!(groups.len(), NUM_ROOTS);
    assert_eq!(total_uris, NUM_PATHS);
}
