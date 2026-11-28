// Copyright (c) 2022 Unfolded Circle ApS, Markus Zehnder <markus.z@unfoldedcircle.com>
// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! Common utility functions.

use serde::de::{DeserializeOwned, Error};
use serde_json::Value;
use std::cmp::Ordering;
use std::env;
use std::ffi::OsStr;

/// Read a boolean from an environment variable. `true` and `1` are true.
///
/// Returns `false` if the variable is not set or has another value.
pub fn bool_from_env<K: AsRef<OsStr>>(key: K) -> bool {
    env::var(key).is_ok_and(|v| v == "true" || v == "1")
}

/// Compare two names: first by the lowercase form of each character, then exactly. Does not
/// allocate.
pub fn cmp_names(a: &str, b: &str) -> Ordering {
    a.chars()
        .flat_map(char::to_lowercase)
        .cmp(b.chars().flat_map(char::to_lowercase))
        .then_with(|| a.cmp(b))
}

/// Deserialize the `msg_data` JSON value of a message into a typed struct.
pub trait DeserializeMsgData: Into<Option<Value>> {
    fn deserialize<T: DeserializeOwned>(self) -> Result<T, serde_json::Error> {
        self.into().map_or_else(
            || Err(serde_json::Error::custom("Missing field: 'msg_data'")),
            T::deserialize,
        )
    }
}

macro_rules! return_fut_ok {
    ($result:expr) => {
        return Box::pin(actix::fut::result(Ok($result)))
    };
}

macro_rules! return_fut_err {
    ($result:expr) => {
        return Box::pin(actix::fut::result(Err($result)))
    };
}

pub(crate) use return_fut_err;
pub(crate) use return_fut_ok;

/// Allocation count for the `dhat_` tests.
#[cfg(all(test, feature = "dhat-heap"))]
pub mod alloc_count {
    use std::sync::{Mutex, PoisonError};

    /// Only one dhat profiler can run at a time.
    static LOCK: Mutex<()> = Mutex::new(());

    /// Run `f` and return its result and the number of allocations it made. Other threads also
    /// count, so run these tests alone.
    pub fn blocks<R>(f: impl FnOnce() -> R) -> (R, u64) {
        let _lock = LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        let _profiler = dhat::Profiler::builder().testing().build();
        let before = dhat::HeapStats::get().total_blocks;
        let result = f();
        let after = dhat::HeapStats::get().total_blocks;
        (result, after - before)
    }
}
