//! Feature-gated instrumentation for one primary payload `Store::put`.
//! Production builds do not compile this module or any of its call sites.

use std::cell::RefCell;
use std::time::{Duration, Instant};

#[derive(Default, Debug, Clone, Copy)]
pub struct PutTrace {
    pub total: Duration,
    pub frame_encode: Duration,
    pub wal_crc: Duration,
    pub wal_buffer_copy: Duration,
    pub wal_flush: Duration,
    pub btree_total: Duration,
    pub record_encode: Duration,
    pub descent: Duration,
    pub leaf_search: Duration,
    pub leaf_insert: Duration,
    pub split: Duration,
    pub page_crc: Duration,
    pub page_crc_count: u64,
    pub leaf_split_count: u64,
    pub interior_split_count: u64,
    /// Full copies of the caller's value bytes on the path from Store::put
    /// into the leaf page. Partial copies and metadata-only copies are not
    /// counted.
    pub value_copies: u64,
}

#[derive(Clone, Copy)]
pub enum Field {
    FrameEncode,
    WalCrc,
    WalBufferCopy,
    WalFlush,
    BtreeTotal,
    RecordEncode,
    Descent,
    LeafSearch,
    LeafInsert,
    Split,
    PageCrc,
}

#[derive(Default)]
struct State {
    active: bool,
    trace: PutTrace,
    payload_active: bool,
    payload_page_crc: Duration,
    payload_page_crc_count: u64,
}

thread_local! { static STATE: RefCell<State> = RefCell::new(State::default()); }

pub fn begin() -> Instant {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        s.active = true;
        s.trace = PutTrace::default();
    });
    Instant::now()
}

pub fn finish(started: Instant) -> PutTrace {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        s.trace.total = started.elapsed();
        s.active = false;
        s.trace
    })
}

pub fn active() -> bool {
    STATE.with(|s| s.borrow().active)
}

pub fn add(field: Field, elapsed: Duration) {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        if matches!(field, Field::PageCrc) && s.payload_active {
            s.payload_page_crc += elapsed;
        }
        if !s.active {
            return;
        }
        let dst = match field {
            Field::FrameEncode => &mut s.trace.frame_encode,
            Field::WalCrc => &mut s.trace.wal_crc,
            Field::WalBufferCopy => &mut s.trace.wal_buffer_copy,
            Field::WalFlush => &mut s.trace.wal_flush,
            Field::BtreeTotal => &mut s.trace.btree_total,
            Field::RecordEncode => &mut s.trace.record_encode,
            Field::Descent => &mut s.trace.descent,
            Field::LeafSearch => &mut s.trace.leaf_search,
            Field::LeafInsert => &mut s.trace.leaf_insert,
            Field::Split => &mut s.trace.split,
            Field::PageCrc => &mut s.trace.page_crc,
        };
        *dst += elapsed;
    });
}

pub fn value_copy() {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        if s.active {
            s.trace.value_copies += 1;
        }
    });
}
pub fn page_crc() {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        if s.payload_active {
            s.payload_page_crc_count += 1;
        }
        if s.active {
            s.trace.page_crc_count += 1;
        }
    });
}

pub fn payload_begin() {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        s.payload_active = true;
        s.payload_page_crc = Duration::ZERO;
        s.payload_page_crc_count = 0;
    });
}

pub fn payload_finish() -> (Duration, u64) {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        s.payload_active = false;
        (s.payload_page_crc, s.payload_page_crc_count)
    })
}
pub fn leaf_split() {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        if s.active {
            s.trace.leaf_split_count += 1;
        }
    });
}
pub fn interior_split() {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        if s.active {
            s.trace.interior_split_count += 1;
        }
    });
}
