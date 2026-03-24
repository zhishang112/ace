//! Stress tests for ace-proxy core components
//!
//! Tests cover:
//! 1. Routing performance under high volume
//! 2. Throttler under event storms
//! 3. JSON-RPC serialization throughput
//! 4. Concurrent pending request map contention
//! 5. LRU cache eviction under rapid backend churn
//! 6. Git root cache hit rate under realistic workloads
//!
//! Run with: `cargo test --test stress_test --release -- --nocapture`

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{oneshot, Mutex};

// ─── Inline minimal types to avoid coupling to internal crate modules ───
// We re-implement lightweight versions of the core structures for testing

/// Simulated JSON-RPC types for stress testing
mod jsonrpc_sim {
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct JsonRpcRequest {
        pub jsonrpc: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub id: Option<JsonRpcId>,
        pub method: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub params: Option<serde_json::Value>,
    }

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
    #[serde(untagged)]
    pub enum JsonRpcId {
        Number(i64),
        String(String),
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct JsonRpcResponse {
        pub jsonrpc: String,
        pub id: Option<JsonRpcId>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub result: Option<serde_json::Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub error: Option<serde_json::Value>,
    }

    impl JsonRpcRequest {
        pub fn new_request(id: i64, method: &str) -> Self {
            Self {
                jsonrpc: "2.0".to_string(),
                id: Some(JsonRpcId::Number(id)),
                method: method.to_string(),
                params: Some(serde_json::json!({"uri": format!("file:///workspace/project/src/file_{}.rs", id)})),
            }
        }

        pub fn new_notification(method: &str) -> Self {
            Self {
                jsonrpc: "2.0".to_string(),
                id: None,
                method: method.to_string(),
                params: None,
            }
        }
    }

    impl JsonRpcResponse {
        pub fn success(id: i64) -> Self {
            Self {
                jsonrpc: "2.0".to_string(),
                id: Some(JsonRpcId::Number(id)),
                result: Some(serde_json::json!({"status": "ok"})),
                error: None,
            }
        }
    }
}

use jsonrpc_sim::*;

// ─── Test 1: JSON-RPC Serialization Throughput ───

#[test]
fn stress_jsonrpc_serialization_throughput() {
    const ITERATIONS: usize = 100_000;

    // Benchmark serialization
    let request = JsonRpcRequest::new_request(1, "tools/call");
    let start = Instant::now();
    for i in 0..ITERATIONS {
        let mut req = request.clone();
        req.id = Some(JsonRpcId::Number(i as i64));
        let json = serde_json::to_string(&req).unwrap();
        assert!(!json.is_empty());
    }
    let ser_elapsed = start.elapsed();

    // Benchmark deserialization
    let sample_json = serde_json::to_string(&request).unwrap();
    let start = Instant::now();
    for _ in 0..ITERATIONS {
        let _req: JsonRpcRequest = serde_json::from_str(&sample_json).unwrap();
    }
    let deser_elapsed = start.elapsed();

    // Benchmark response serialization
    let response = JsonRpcResponse::success(1);
    let start = Instant::now();
    for _ in 0..ITERATIONS {
        let json = serde_json::to_string(&response).unwrap();
        assert!(!json.is_empty());
    }
    let resp_elapsed = start.elapsed();

    let ser_rate = ITERATIONS as f64 / ser_elapsed.as_secs_f64();
    let deser_rate = ITERATIONS as f64 / deser_elapsed.as_secs_f64();
    let resp_rate = ITERATIONS as f64 / resp_elapsed.as_secs_f64();

    println!("=== JSON-RPC Serialization Throughput ===");
    println!("  Request serialize:   {:.0} ops/sec ({:.2}ms total)", ser_rate, ser_elapsed.as_secs_f64() * 1000.0);
    println!("  Request deserialize: {:.0} ops/sec ({:.2}ms total)", deser_rate, deser_elapsed.as_secs_f64() * 1000.0);
    println!("  Response serialize:  {:.0} ops/sec ({:.2}ms total)", resp_rate, resp_elapsed.as_secs_f64() * 1000.0);

    // Minimum performance threshold: 100k ops/sec in debug mode, much higher in release
    assert!(ser_rate > 50_000.0, "Serialization too slow: {:.0} ops/sec", ser_rate);
    assert!(deser_rate > 50_000.0, "Deserialization too slow: {:.0} ops/sec", deser_rate);
}

// ─── Test 2: Pending Request Map Contention ───

#[tokio::test]
async fn stress_pending_map_concurrent_insert_remove() {
    const NUM_TASKS: usize = 50;
    const OPS_PER_TASK: usize = 1000;

    struct PendingEntry {
        _tx: oneshot::Sender<JsonRpcResponse>,
    }

    let pending: Arc<Mutex<HashMap<u64, PendingEntry>>> = Arc::new(Mutex::new(HashMap::new()));
    let counter = Arc::new(AtomicU64::new(0));

    let start = Instant::now();

    let mut handles = Vec::new();
    for _ in 0..NUM_TASKS {
        let pending = pending.clone();
        let counter = counter.clone();
        handles.push(tokio::spawn(async move {
            for _ in 0..OPS_PER_TASK {
                let id = counter.fetch_add(1, Ordering::SeqCst);
                let (tx, _rx) = oneshot::channel();

                // Insert
                {
                    let mut map = pending.lock().await;
                    map.insert(id, PendingEntry { _tx: tx });
                }

                // Small yield to simulate async work
                tokio::task::yield_now().await;

                // Remove
                {
                    let mut map = pending.lock().await;
                    map.remove(&id);
                }
            }
        }));
    }

    for h in handles {
        h.await.unwrap();
    }

    let elapsed = start.elapsed();
    let total_ops = NUM_TASKS * OPS_PER_TASK * 2; // insert + remove
    let ops_per_sec = total_ops as f64 / elapsed.as_secs_f64();

    println!("=== Pending Map Contention ===");
    println!("  {} tasks × {} ops = {} total lock operations", NUM_TASKS, OPS_PER_TASK, total_ops);
    println!("  Elapsed: {:.2}ms", elapsed.as_secs_f64() * 1000.0);
    println!("  Throughput: {:.0} ops/sec", ops_per_sec);

    // Must complete without deadlocks
    let map = pending.lock().await;
    assert!(map.is_empty(), "Pending map should be empty, has {} entries", map.len());
}

// ─── Test 3: Oneshot Channel Pressure (simulates request/response flow) ───

#[tokio::test]
async fn stress_request_response_channel_pressure() {
    const NUM_REQUESTS: usize = 10_000;

    let start = Instant::now();
    let mut handles = Vec::with_capacity(NUM_REQUESTS);

    for i in 0..NUM_REQUESTS {
        let (tx, rx) = oneshot::channel::<JsonRpcResponse>();

        // Simulate backend responding
        let handle = tokio::spawn(async move {
            let response = JsonRpcResponse::success(i as i64);
            tx.send(response).unwrap();
        });

        handles.push((rx, handle));
    }

    let mut success_count = 0;
    for (rx, handle) in handles {
        let response = tokio::time::timeout(Duration::from_secs(5), rx).await;
        match response {
            Ok(Ok(resp)) => {
                assert_eq!(resp.jsonrpc, "2.0");
                success_count += 1;
            }
            Ok(Err(_)) => panic!("Channel closed unexpectedly"),
            Err(_) => panic!("Timeout waiting for response"),
        }
        handle.await.unwrap();
    }

    let elapsed = start.elapsed();
    let rate = NUM_REQUESTS as f64 / elapsed.as_secs_f64();

    println!("=== Request/Response Channel Pressure ===");
    println!("  {} requests completed in {:.2}ms", NUM_REQUESTS, elapsed.as_secs_f64() * 1000.0);
    println!("  Throughput: {:.0} req/sec", rate);
    println!("  Success: {}/{}", success_count, NUM_REQUESTS);

    assert_eq!(success_count, NUM_REQUESTS);
}

// ─── Test 4: Throttler Event Storm ───

#[test]
fn stress_throttler_event_storm() {
    use std::collections::HashSet;

    const NUM_EVENTS: usize = 50_000;
    const UNIQUE_PATHS: usize = 5_000;

    // Simulate EventThrottler behavior
    let mut pending_paths: HashSet<PathBuf> = HashSet::new();

    let start = Instant::now();

    // Flood with events (many duplicates)
    for i in 0..NUM_EVENTS {
        let path_idx = i % UNIQUE_PATHS;
        let path = PathBuf::from(format!("/workspace/project/src/module_{}/file_{}.rs", path_idx / 100, path_idx));
        pending_paths.insert(path);
    }

    assert_eq!(pending_paths.len(), UNIQUE_PATHS, "Deduplication failed");

    // Simulate flush: drain all paths
    let flushed: Vec<PathBuf> = pending_paths.drain().collect();
    assert_eq!(flushed.len(), UNIQUE_PATHS);

    let elapsed = start.elapsed();
    let rate = NUM_EVENTS as f64 / elapsed.as_secs_f64();

    println!("=== Throttler Event Storm ===");
    println!("  {} events → {} unique paths ({}x dedup ratio)", NUM_EVENTS, UNIQUE_PATHS, NUM_EVENTS / UNIQUE_PATHS);
    println!("  Elapsed: {:.2}ms", elapsed.as_secs_f64() * 1000.0);
    println!("  Insert throughput: {:.0} events/sec", rate);
}

// ─── Test 5: Routing — Longest Prefix Match Performance ───

#[test]
fn stress_routing_longest_prefix_match() {
    const NUM_ROOTS: usize = 20;
    const NUM_LOOKUPS: usize = 100_000;

    // Build roots with varying depth
    let roots: Vec<PathBuf> = (0..NUM_ROOTS)
        .map(|i| PathBuf::from(format!("/workspace/project_{}/src", i)))
        .collect();

    // Build lookup paths
    let lookup_paths: Vec<PathBuf> = (0..NUM_LOOKUPS)
        .map(|i| {
            let root_idx = i % NUM_ROOTS;
            PathBuf::from(format!(
                "/workspace/project_{}/src/module/deep/path/file_{}.rs",
                root_idx, i
            ))
        })
        .collect();

    let start = Instant::now();
    let mut match_count = 0;

    for path in &lookup_paths {
        // This is exactly the algorithm used in determine_root()
        let matched = roots
            .iter()
            .filter(|root| path.starts_with(root))
            .max_by_key(|root| root.as_os_str().len())
            .cloned();

        if matched.is_some() {
            match_count += 1;
        }
    }

    let elapsed = start.elapsed();
    let rate = NUM_LOOKUPS as f64 / elapsed.as_secs_f64();

    println!("=== Routing Longest Prefix Match ===");
    println!("  {} roots × {} lookups", NUM_ROOTS, NUM_LOOKUPS);
    println!("  Matched: {}/{}", match_count, NUM_LOOKUPS);
    println!("  Elapsed: {:.2}ms", elapsed.as_secs_f64() * 1000.0);
    println!("  Throughput: {:.0} lookups/sec", rate);

    assert_eq!(match_count, NUM_LOOKUPS, "All paths should match a root");
    // With 20 roots, even naive O(n) should handle 100k+ lookups/sec
    assert!(rate > 100_000.0, "Routing too slow: {:.0} lookups/sec", rate);
}

// ─── Test 6: Git Root Cache Hit Rate ───

#[test]
fn stress_git_root_cache_hit_rate() {
    const NUM_LOOKUPS: usize = 100_000;
    const UNIQUE_DIRS: usize = 500;

    let mut cache: HashMap<PathBuf, Option<PathBuf>> = HashMap::new();
    let mut hit_count = 0u64;
    let mut miss_count = 0u64;

    let start = Instant::now();

    for i in 0..NUM_LOOKUPS {
        let dir_idx = i % UNIQUE_DIRS;
        let cache_key = PathBuf::from(format!("/workspace/project/src/module_{}", dir_idx));

        if cache.contains_key(&cache_key) {
            hit_count += 1;
            let _cached = cache.get(&cache_key);
        } else {
            miss_count += 1;
            // Simulate find_git_root() discovering the root
            let result = Some(PathBuf::from("/workspace/project"));
            cache.insert(cache_key, result);
        }
    }

    let elapsed = start.elapsed();
    let hit_rate = hit_count as f64 / NUM_LOOKUPS as f64 * 100.0;
    let rate = NUM_LOOKUPS as f64 / elapsed.as_secs_f64();

    println!("=== Git Root Cache ===");
    println!("  {} lookups, {} unique dirs", NUM_LOOKUPS, UNIQUE_DIRS);
    println!("  Hits: {} ({:.1}%), Misses: {}", hit_count, hit_rate, miss_count);
    println!("  Elapsed: {:.2}ms", elapsed.as_secs_f64() * 1000.0);
    println!("  Throughput: {:.0} lookups/sec", rate);

    assert_eq!(miss_count as usize, UNIQUE_DIRS, "Should miss exactly once per unique dir");
    assert!(hit_rate > 99.0, "Cache hit rate too low: {:.1}%", hit_rate);
}

// ─── Test 7: LRU Cache Eviction Under Backend Churn ───

#[test]
fn stress_lru_eviction_under_churn() {
    use lru::LruCache;
    use std::num::NonZeroUsize;

    const CACHE_SIZE: usize = 5;
    const NUM_ROOTS: usize = 50;
    const ACCESS_ROUNDS: usize = 10_000;

    let mut cache: LruCache<PathBuf, String> = LruCache::new(NonZeroUsize::new(CACHE_SIZE).unwrap());
    let roots: Vec<PathBuf> = (0..NUM_ROOTS)
        .map(|i| PathBuf::from(format!("/project_{}", i)))
        .collect();

    let mut eviction_count = 0u64;
    let mut hit_count = 0u64;

    let start = Instant::now();

    for round in 0..ACCESS_ROUNDS {
        // Simulate realistic access pattern: 80% hot set, 20% cold
        let root_idx = if round % 5 == 0 {
            // Cold path — random from full range
            round % NUM_ROOTS
        } else {
            // Hot path — concentrated on first few
            round % (CACHE_SIZE.min(NUM_ROOTS))
        };

        let root = &roots[root_idx];

        if cache.get(root).is_some() {
            hit_count += 1;
        } else {
            // Simulate creating a backend
            if cache.len() == CACHE_SIZE {
                eviction_count += 1;
            }
            cache.put(root.clone(), format!("backend_{}", root_idx));
        }
    }

    let elapsed = start.elapsed();
    let hit_rate = hit_count as f64 / ACCESS_ROUNDS as f64 * 100.0;
    let rate = ACCESS_ROUNDS as f64 / elapsed.as_secs_f64();

    println!("=== LRU Cache Eviction Under Churn ===");
    println!("  Cache size: {}, Unique roots: {}, Rounds: {}", CACHE_SIZE, NUM_ROOTS, ACCESS_ROUNDS);
    println!("  Hits: {} ({:.1}%), Evictions: {}", hit_count, hit_rate, eviction_count);
    println!("  Elapsed: {:.2}ms", elapsed.as_secs_f64() * 1000.0);
    println!("  Throughput: {:.0} ops/sec", rate);

    // Hot set should give decent hit rate with 80/20 access pattern
    assert!(hit_rate > 50.0, "LRU hit rate too low for 80/20 pattern: {:.1}%", hit_rate);
}

// ─── Test 8: Concurrent Backend Crash + Fast Fail Simulation ───

#[tokio::test]
async fn stress_concurrent_crash_fast_fail() {
    const NUM_PENDING: usize = 200;

    // Simulate pending request map
    let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<JsonRpcResponse>>>> =
        Arc::new(Mutex::new(HashMap::new()));

    // Create a bunch of pending requests
    let mut receivers = Vec::with_capacity(NUM_PENDING);
    {
        let mut map = pending.lock().await;
        for i in 0..NUM_PENDING {
            let (tx, rx) = oneshot::channel();
            map.insert(i as u64, tx);
            receivers.push(rx);
        }
    }

    assert_eq!(pending.lock().await.len(), NUM_PENDING);

    let start = Instant::now();

    // Simulate crash: drain all pending and send error responses
    {
        let mut map = pending.lock().await;
        let count = map.len();
        for (id, tx) in map.drain() {
            let err_response = JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id: Some(JsonRpcId::Number(id as i64)),
                result: None,
                error: Some(serde_json::json!({
                    "code": -32001,
                    "message": "Backend process crashed"
                })),
            };
            let _ = tx.send(err_response);
        }
        assert_eq!(count, NUM_PENDING);
    }

    // Verify all receivers got error responses
    let mut error_count = 0;
    for rx in receivers {
        match rx.await {
            Ok(resp) => {
                assert!(resp.error.is_some());
                error_count += 1;
            }
            Err(_) => panic!("Channel closed without sending response"),
        }
    }

    let elapsed = start.elapsed();

    println!("=== Concurrent Crash Fast Fail ===");
    println!("  {} pending requests drained in {:.2}ms", NUM_PENDING, elapsed.as_secs_f64() * 1000.0);
    println!("  All {} receivers got error response", error_count);

    assert_eq!(error_count, NUM_PENDING);
    // Fast fail should resolve in < 50ms
    assert!(elapsed < Duration::from_millis(50), "Fast fail too slow: {:?}", elapsed);
}

// ─── Test 9: URI to Path Parsing Performance ───

#[test]
fn stress_uri_to_path_parsing() {
    use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};

    const NUM_URIS: usize = 100_000;

    // Generate realistic URIs
    let uris: Vec<String> = (0..NUM_URIS)
        .map(|i| {
            let path = format!("C:/Users/dev/workspace/project/src/module_{}/file_{}.rs", i % 100, i);
            format!("file:///{}", utf8_percent_encode(&path, NON_ALPHANUMERIC))
        })
        .collect();

    let start = Instant::now();
    let mut success_count = 0;

    for uri in &uris {
        // Simulate uri_to_path logic
        if let Some(path_str) = uri.strip_prefix("file:///") {
            let decoded = percent_encoding::percent_decode_str(path_str)
                .decode_utf8()
                .ok();
            if decoded.is_some() {
                success_count += 1;
            }
        }
    }

    let elapsed = start.elapsed();
    let rate = NUM_URIS as f64 / elapsed.as_secs_f64();

    println!("=== URI to Path Parsing ===");
    println!("  {} URIs parsed in {:.2}ms", NUM_URIS, elapsed.as_secs_f64() * 1000.0);
    println!("  Success: {}/{}", success_count, NUM_URIS);
    println!("  Throughput: {:.0} ops/sec", rate);

    assert_eq!(success_count, NUM_URIS);
    assert!(rate > 100_000.0, "URI parsing too slow: {:.0} ops/sec", rate);
}

// ─── Test 10: mpsc Channel Backpressure (stdin writer simulation) ───

#[tokio::test]
async fn stress_mpsc_channel_backpressure() {
    const CHANNEL_SIZE: usize = 100;
    const NUM_MESSAGES: usize = 50_000;

    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(CHANNEL_SIZE);

    let start = Instant::now();

    // Producer: sends many messages
    let producer = tokio::spawn(async move {
        for i in 0..NUM_MESSAGES {
            let msg = format!(r#"{{"jsonrpc":"2.0","id":{},"method":"tools/call","params":{{}}}}"#, i);
            tx.send(msg).await.unwrap();
        }
    });

    // Consumer: drains messages (simulates stdin writer)
    let consumer = tokio::spawn(async move {
        let mut count = 0;
        while let Some(msg) = rx.recv().await {
            assert!(!msg.is_empty());
            count += 1;
            if count >= NUM_MESSAGES {
                break;
            }
        }
        count
    });

    producer.await.unwrap();
    let consumed = consumer.await.unwrap();

    let elapsed = start.elapsed();
    let rate = NUM_MESSAGES as f64 / elapsed.as_secs_f64();

    println!("=== mpsc Channel Backpressure ===");
    println!("  Channel size: {}, Messages: {}", CHANNEL_SIZE, NUM_MESSAGES);
    println!("  Consumed: {}", consumed);
    println!("  Elapsed: {:.2}ms", elapsed.as_secs_f64() * 1000.0);
    println!("  Throughput: {:.0} msg/sec", rate);

    assert_eq!(consumed, NUM_MESSAGES);
}

// ─── Test 11: Path Grouping Performance (flush_throttled_events path) ───

#[test]
fn stress_path_grouping_by_root() {
    const NUM_ROOTS: usize = 10;
    const NUM_PATHS: usize = 50_000;

    let roots: Vec<PathBuf> = (0..NUM_ROOTS)
        .map(|i| PathBuf::from(format!("/workspace/project_{}", i)))
        .collect();

    let paths: Vec<PathBuf> = (0..NUM_PATHS)
        .map(|i| {
            let root_idx = i % NUM_ROOTS;
            PathBuf::from(format!("/workspace/project_{}/src/file_{}.rs", root_idx, i))
        })
        .collect();

    let start = Instant::now();

    // Exactly mirrors routing::group_paths_by_root logic
    let mut paths_by_root: HashMap<PathBuf, Vec<String>> = HashMap::new();
    for path in &paths {
        let root = roots
            .iter()
            .filter(|r| path.starts_with(r))
            .max_by_key(|r| r.as_os_str().len())
            .cloned();

        if let Some(root) = root {
            let uri = format!("file:///{}", path.display().to_string().replace('\\', "/"));
            paths_by_root.entry(root).or_default().push(uri);
        }
    }

    let elapsed = start.elapsed();
    let rate = NUM_PATHS as f64 / elapsed.as_secs_f64();

    println!("=== Path Grouping by Root ===");
    println!("  {} paths grouped into {} roots", NUM_PATHS, paths_by_root.len());
    for (root, uris) in &paths_by_root {
        println!("    {}: {} uris", root.display(), uris.len());
    }
    println!("  Elapsed: {:.2}ms", elapsed.as_secs_f64() * 1000.0);
    println!("  Throughput: {:.0} paths/sec", rate);

    assert_eq!(paths_by_root.len(), NUM_ROOTS);
    assert!(rate > 100_000.0, "Path grouping too slow: {:.0} paths/sec", rate);
}
