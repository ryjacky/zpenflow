//! Single-producer-single-consumer queue with drop-oldest overflow.
//!
//! The pipeline thread (encoder) pushes; the server (transport) pops. We
//! prefer to drop the OLDEST encoded packet on overflow — a backed-up consumer
//! generally means the network is slow, and an older queued frame is staler
//! than the newest one.
//!
//! Default capacity 8 ≈ 130 ms at 60 fps, which is well past the e2e budget;
//! once the queue is fuller than that the consumer is failing in a way the
//! application cares about. Drop-oldest gives the consumer best-effort
//! freshness without unbounded memory growth.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

pub struct PacketQueue<T> {
    inner: Mutex<Inner<T>>,
    cv: Condvar,
}

struct Inner<T> {
    deque: VecDeque<T>,
    capacity: usize,
    closed: bool,
    /// Counter of packets that the producer queued but were dropped before a
    /// consumer could read them. The pipeline can surface this as telemetry.
    dropped_overflow: u64,
}

impl<T> PacketQueue<T> {
    pub fn new(capacity: usize) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Inner {
                deque: VecDeque::with_capacity(capacity.max(1)),
                capacity: capacity.max(1),
                closed: false,
                dropped_overflow: 0,
            }),
            cv: Condvar::new(),
        })
    }

    /// Push a packet. If the queue is at capacity, drops the OLDEST packet to
    /// make room (HANDOFF design choice — for video, freshness wins over
    /// completeness). Returns the number of overflow drops accumulated so far.
    pub fn push(&self, item: T) -> u64 {
        let mut g = self.inner.lock().unwrap();
        if g.closed {
            return g.dropped_overflow;
        }
        if g.deque.len() == g.capacity {
            let _ = g.deque.pop_front();
            g.dropped_overflow = g.dropped_overflow.saturating_add(1);
        }
        g.deque.push_back(item);
        let dropped = g.dropped_overflow;
        drop(g);
        self.cv.notify_one();
        dropped
    }

    /// Block until a packet is available or the queue is closed.
    pub fn pop_blocking(&self) -> Option<T> {
        let mut g = self.inner.lock().unwrap();
        loop {
            if let Some(item) = g.deque.pop_front() {
                return Some(item);
            }
            if g.closed {
                return None;
            }
            g = self.cv.wait(g).unwrap();
        }
    }

    /// Block up to `timeout`. Returns Some(packet), or None on timeout/close.
    pub fn pop_timeout(&self, timeout: Duration) -> Option<T> {
        let deadline = Instant::now() + timeout;
        let mut g = self.inner.lock().unwrap();
        loop {
            if let Some(item) = g.deque.pop_front() {
                return Some(item);
            }
            if g.closed {
                return None;
            }
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            let (g2, _) = self.cv.wait_timeout(g, deadline - now).unwrap();
            g = g2;
        }
    }

    /// Wake all consumers; subsequent push() calls become no-ops.
    pub fn close(&self) {
        let mut g = self.inner.lock().unwrap();
        g.closed = true;
        drop(g);
        self.cv.notify_all();
    }

    /// Snapshot of (depth, capacity, total dropped). Cheap, lock-acquired.
    pub fn stats(&self) -> QueueStats {
        let g = self.inner.lock().unwrap();
        QueueStats {
            depth: g.deque.len(),
            capacity: g.capacity,
            dropped_overflow: g.dropped_overflow,
            closed: g.closed,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct QueueStats {
    pub depth: usize,
    pub capacity: usize,
    pub dropped_overflow: u64,
    pub closed: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn push_pop_basic() {
        let q = PacketQueue::<u32>::new(4);
        q.push(1);
        q.push(2);
        assert_eq!(q.pop_blocking(), Some(1));
        assert_eq!(q.pop_blocking(), Some(2));
    }

    #[test]
    fn drop_oldest_on_overflow() {
        let q = PacketQueue::<u32>::new(2);
        q.push(1);
        q.push(2);
        q.push(3); // 1 is dropped
        q.push(4); // 2 is dropped
        assert_eq!(q.pop_blocking(), Some(3));
        assert_eq!(q.pop_blocking(), Some(4));
        assert_eq!(q.stats().dropped_overflow, 2);
    }

    #[test]
    fn close_unblocks_consumers() {
        let q = PacketQueue::<u32>::new(4);
        let q2 = q.clone();
        let h = thread::spawn(move || q2.pop_blocking());
        thread::sleep(Duration::from_millis(20));
        q.close();
        assert_eq!(h.join().unwrap(), None);
    }

    #[test]
    fn pop_timeout_returns_none() {
        let q = PacketQueue::<u32>::new(4);
        assert_eq!(q.pop_timeout(Duration::from_millis(20)), None);
    }
}
