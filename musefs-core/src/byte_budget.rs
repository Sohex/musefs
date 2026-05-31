//! A byte-accounted semaphore: producers reserve N bytes before holding a value,
//! blocking until the in-flight total would stay within the cap; the consumer
//! releases bytes after persisting. Bounds peak in-flight art memory.

use std::sync::{Condvar, Mutex};

pub struct ByteBudget {
    cap: u64,
    state: Mutex<u64>,
    cv: Condvar,
}

impl ByteBudget {
    pub fn new(cap: u64) -> Self {
        Self {
            cap,
            state: Mutex::new(0),
            cv: Condvar::new(),
        }
    }

    /// Reserve `n` bytes, blocking until they fit (a single item larger than the
    /// cap is admitted alone once in-flight is zero, to guarantee progress).
    pub fn acquire(&self, n: u64) {
        let mut in_flight = self.state.lock().unwrap();
        while *in_flight != 0 && *in_flight + n > self.cap {
            in_flight = self.cv.wait(in_flight).unwrap();
        }
        *in_flight += n;
    }

    /// Release `n` previously reserved bytes.
    pub fn release(&self, n: u64) {
        let mut in_flight = self.state.lock().unwrap();
        *in_flight = in_flight.saturating_sub(n);
        self.cv.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn oversized_item_admitted_when_idle() {
        let b = ByteBudget::new(10);
        b.acquire(1000); // larger than cap, but in-flight was 0 → admitted
        b.release(1000);
    }

    #[test]
    fn blocks_until_release() {
        let b = Arc::new(ByteBudget::new(10));
        b.acquire(10);
        let b2 = Arc::clone(&b);
        let h = std::thread::spawn(move || b2.acquire(5)); // must block
        std::thread::sleep(std::time::Duration::from_millis(50));
        b.release(10); // unblocks the spawned acquire
        h.join().unwrap();
        b.release(5);
    }
}
