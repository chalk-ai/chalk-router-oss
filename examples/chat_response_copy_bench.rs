//! Counts only caller-side trace ownership and HTTP body construction, not
//! response parsing, writer/exporter internals, cache retention, or networking.
//! The metadata gate itself is covered by trace_attributes unit tests.
//! Run after implementation with `just bench-chat-copies`.
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Instant;

use axum::body::{Body, Bytes};
use serde_json::value::RawValue;

struct CountingAllocator;
static ENABLED: AtomicBool = AtomicBool::new(false);
static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
static BYTES: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if ENABLED.load(Ordering::Relaxed) {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
            BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        }
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        if ENABLED.load(Ordering::Relaxed) {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
            BYTES.fetch_add(size, Ordering::Relaxed);
        }
        unsafe { System.realloc(ptr, layout, size) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

fn main() {
    for content_bytes in [256, 96 * 1024, 768 * 1024] {
        let wire = format!(r#"{{"content":"{}"}}"#, "x".repeat(content_bytes));
        let template = RawValue::from_string(wire).unwrap();
        for log_content in [false, true] {
            for after in [false, true] {
                let mut allocations = 0;
                let mut bytes = 0;
                let mut ns = 0;
                for _ in 0..100 {
                    // Parsing/preparing the upstream raw allocation is outside
                    // the measurement; both paths start with the same ownership.
                    let raw = template.clone();
                    let original_ptr = raw.get().as_ptr();
                    ALLOCATIONS.store(0, Ordering::Relaxed);
                    BYTES.store(0, Ordering::Relaxed);
                    ENABLED.store(true, Ordering::Relaxed);
                    let started = Instant::now();
                    let trace = if after {
                        log_content.then(|| raw.get().to_owned())
                    } else {
                        let owned = raw.get().to_owned();
                        // Before, the writer refused content only after the
                        // caller copied it. Keep that ownership work observable.
                        std::hint::black_box(&owned);
                        log_content.then_some(owned)
                    };
                    let body = if after {
                        let raw: Box<str> = raw.into();
                        let bytes = Bytes::from(String::from(raw));
                        assert_eq!(original_ptr, bytes.as_ptr());
                        Body::from(bytes)
                    } else {
                        Body::from(raw.get().as_bytes().to_vec())
                    };
                    std::hint::black_box((body, trace));
                    ns += started.elapsed().as_nanos();
                    ENABLED.store(false, Ordering::Relaxed);
                    allocations += ALLOCATIONS.load(Ordering::Relaxed);
                    bytes += BYTES.load(Ordering::Relaxed);
                }
                println!(
                    "{{\"wire_bytes\":{},\"content\":{},\"after\":{},\"allocations\":{},\"allocated_bytes\":{},\"mean_ns\":{}}}",
                    template.get().len(), log_content, after, allocations / 100, bytes / 100, ns / 100
                );
            }
        }
    }
}
