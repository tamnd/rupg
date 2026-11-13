// Lifted from rudb crates/rudb-qc/src/tier/cache.rs at cd9f9676 (2026-10-05).
// Changes: a module of rupg-jit, so QIR is `crate::ir` and the runtime is `crate::rt`.

//! The function cache, level L2 of section 9.8 of `spec/compiler/09-tiering-and-caching.md`.
//!
//! Machine code is kept per process, keyed on the backend and the whole function with its name,
//! version and plan number left out, so the same pipeline in another query, or in the same query
//! run again, takes the code instead of compiling it. The key is the whole function and a lookup
//! compares all of it, so two functions that differ in anything a backend could read never share
//! code. It used to be the function's `Debug` text, which took longer to write out for each
//! function of each query than hashing and comparing the function does. The cache holds at most
//! [`BUDGET`] bytes of code and forgets the entry used longest ago past that. A query holds the
//! code it took by an `Arc`, so forgetting an entry never pulls code out from under a query that
//! runs it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use crate::ir::Func;
use crate::rt::code::Code;

/// The bytes of code the cache keeps, `qc.code_cache_bytes` of the spec.
pub(crate) const BUDGET: usize = 64 << 20;

/// What a function's code is found by: the backend that made it and every part of the function
/// the backend reads.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Key {
    backend: &'static str,
    func: Func,
}

/// The key of `f`'s code from `backend`.
pub(crate) fn key(backend: &'static str, f: &Func) -> Key {
    let func = Func { name: String::new(), version: String::new(), plan: 0, ..f.clone() };
    Key { backend, func }
}

struct Entry {
    code: Arc<Code>,
    bytes: usize,
    used: u64,
}

#[derive(Default)]
struct Cache {
    entries: HashMap<Key, Entry>,
    bytes: usize,
    clock: u64,
}

static CACHE: Mutex<Option<Cache>> = Mutex::new(None);

/// The code cached under `key`, if any.
pub(crate) fn get(key: &Key) -> Option<Arc<Code>> {
    let mut cache = CACHE.lock().unwrap_or_else(PoisonError::into_inner);
    let cache = cache.get_or_insert_with(Cache::default);
    cache.clock += 1;
    let clock = cache.clock;
    let entry = cache.entries.get_mut(key)?;
    entry.used = clock;
    Some(Arc::clone(&entry.code))
}

/// Keeps `code` under `key`, and forgets the entries used longest ago while the cache is over its
/// budget. Code bigger than the whole budget is not kept.
pub(crate) fn put(key: Key, code: &Arc<Code>) {
    let bytes = code.len();
    if bytes > BUDGET {
        return;
    }
    let mut cache = CACHE.lock().unwrap_or_else(PoisonError::into_inner);
    let cache = cache.get_or_insert_with(Cache::default);
    cache.clock += 1;
    let entry = Entry { code: Arc::clone(code), bytes, used: cache.clock };
    if let Some(old) = cache.entries.insert(key, entry) {
        cache.bytes -= old.bytes;
    }
    cache.bytes += bytes;
    while cache.bytes > BUDGET {
        let Some(oldest) = cache.entries.iter().min_by_key(|(_, e)| e.used).map(|(k, _)| k.clone())
        else {
            break;
        };
        if let Some(gone) = cache.entries.remove(&oldest) {
            cache.bytes -= gone.bytes;
        }
    }
}
