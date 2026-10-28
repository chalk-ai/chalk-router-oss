//! Run with `just schema-json-bench` before and after a change.
//! Synthetic local data only; no provider calls. Timings are informational.
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use openai_server::{
    prompt_cache::{chat_cache_key, CacheIdentity, CacheScope},
    responses::ResponsesRequest,
    schema_json::SchemaJson,
    types::ChatCompletionRequest,
};
use serde_json::json;

struct CountingAllocator;
static BYTES: AtomicUsize = AtomicUsize::new(0);
static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        BYTES.fetch_add(size, Ordering::Relaxed);
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, size) }
    }
}
#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

fn measure(label: &str, mut operation: impl FnMut()) {
    operation();
    BYTES.store(0, Ordering::Relaxed);
    ALLOCATIONS.store(0, Ordering::Relaxed);
    let start = Instant::now();
    for _ in 0..8 {
        operation();
    }
    println!(
        "{label}: us={} allocations={} bytes={}",
        start.elapsed().as_micros() / 8,
        ALLOCATIONS.load(Ordering::Relaxed) / 8,
        BYTES.load(Ordering::Relaxed) / 8
    );
}
fn main() {
    for size in [64 * 1024, 1024 * 1024, 8 * 1024 * 1024] {
        let body: SchemaJson =
            serde_json::from_value(json!({"model":"public", "input":"x".repeat(size)})).unwrap();
        measure(&format!("body-{size}-edit"), || {
            let mut request = body.clone();
            for n in 0..8 {
                request
                    .set_member("model", json!(format!("route-{n}")).into())
                    .unwrap();
            }
            std::hint::black_box(request);
        });
        measure(&format!("body-{size}-wire"), || {
            let mut request = body.clone();
            request.set_member("model", json!("route").into()).unwrap();
            std::hint::black_box(serde_json::to_vec(&request).unwrap());
        });
        let request: ResponsesRequest =
            serde_json::from_value(json!({"model":"public", "input":"x".repeat(size)})).unwrap();
        request.to_chat_completion().unwrap();
        for attempts in [1, 4, 8] {
            measure(&format!("body-{size}-fallback-{attempts}"), || {
                for n in 0..attempts {
                    let mut candidate = request.clone();
                    candidate.set_model(format!("route-{n}"));
                    std::hint::black_box(candidate.to_chat_completion().unwrap());
                }
            });
        }
    }
    for depth in [16, 32, 64] {
        let mut schema = json!({"type":"object", "properties":{"z":{"type":"string"},"a":{"type":"number","minimum":1.0}}});
        for _ in 0..depth {
            schema = json!({"type":"object", "properties":{"nested":schema}});
        }
        let wire = serde_json::to_string(&schema).unwrap();
        // Each logical schema level adds two JSON containers. The depth-64
        // fixture exceeds the standard ingress limit and is constructed only.
        if depth <= 32 {
            measure(&format!("retained-depth-{depth}-normalize-cold"), || {
                let mut request: SchemaJson = serde_json::from_str(&wire).unwrap();
                request.normalize_strict().unwrap();
                std::hint::black_box(serde_json::to_vec(&request).unwrap());
            });
            measure(&format!("retained-depth-{depth}-canonical-cold"), || {
                let request: SchemaJson = serde_json::from_str(&wire).unwrap();
                std::hint::black_box(
                    serde_json::to_vec(&request.canonical_for_cache().unwrap()).unwrap(),
                );
            });
        }
        let schema: SchemaJson = schema.into();
        measure(&format!("depth-{depth}-normalize"), || {
            let mut normalized = schema.clone();
            normalized.normalize_strict().unwrap();
            std::hint::black_box(serde_json::to_vec(&normalized).unwrap());
        });
        measure(&format!("depth-{depth}-canonical"), || {
            std::hint::black_box(schema.canonical_for_cache().unwrap());
        });
    }
    let identity = CacheIdentity {
        environment_id: "env",
        usage_pool_id: None,
        key_hash: "hash",
    };
    for size in [1024 * 1024, 8 * 1024 * 1024] {
        let request: ChatCompletionRequest = serde_json::from_value(json!({
            "model": "public",
            "messages": [
                {"role": "system", "content": "x".repeat(size / 2)},
                {"role": "user", "content": "y".repeat(size / 2)},
            ],
            "tools": [{"type": "function", "function": {
                "name": "lookup",
                "parameters": {"type": "object", "properties": {
                    "zeta": {"type": "string"},
                    "alpha": {"type": "integer"},
                }}
            }}],
        }))
        .unwrap();
        measure(&format!("chat-cache-key-{size}"), || {
            std::hint::black_box(chat_cache_key(CacheScope::Token, &identity, &request).unwrap());
        });
    }
}
