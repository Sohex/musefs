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
        // `saturating_add` mirrors `release`'s saturating style; art weights are
        // file-bounded so this never saturates in practice, but it keeps the
        // guard total-order-safe regardless.
        while *in_flight != 0 && in_flight.saturating_add(n) > self.cap {
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

    /// Spawn `acquire(n)` and report whether it completed within `wait` (i.e. did
    /// NOT block). Always returns the join handle so the caller can drain it after
    /// releasing the held budget.
    fn acquire_completes_within(
        b: &Arc<ByteBudget>,
        n: u64,
        wait: std::time::Duration,
    ) -> (bool, std::thread::JoinHandle<()>) {
        use std::sync::atomic::{AtomicBool, Ordering};
        let done = Arc::new(AtomicBool::new(false));
        let b2 = Arc::clone(b);
        let done2 = Arc::clone(&done);
        let h = std::thread::spawn(move || {
            b2.acquire(n);
            done2.store(true, Ordering::SeqCst);
        });
        std::thread::sleep(wait);
        (done.load(Ordering::SeqCst), h)
    }

    /// Accumulation must be additive: from idle, `acquire(4)` then `acquire(7)`
    /// (4+7=11 > cap 10) MUST block. This pins `*in_flight += n` (line 32):
    ///   - no-op (`acquire` body → `()`): in_flight stays 0, so the 2nd acquire
    ///     sees 0 and admits 7 (7 <= 10) → would NOT block → killed.
    ///   - `+=`→`*=`: first acquire makes 0*4 = 0, so in_flight stays 0 and the
    ///     2nd acquire admits → would NOT block → killed.
    // kills byte_budget L24 acquire→no-op and L32 `+=`→`*=`
    #[test]
    fn accumulates_additively_then_blocks() {
        let wait = std::time::Duration::from_millis(100);
        let b = Arc::new(ByteBudget::new(10));
        b.acquire(4); // in_flight = 4 (must be > 0 and == 4 for the next to block)
        let (completed, h) = acquire_completes_within(&b, 7, wait);
        assert!(
            !completed,
            "4+7=11 > cap 10 must block; if it completed, accounting is not additive"
        );
        b.release(4); // unblocks the spawned acquire(7)
        h.join().unwrap();
        b.release(7);
    }

    /// Boundary: `in_flight + n == cap` is admitted (guard is strictly `> cap`).
    /// From idle, `acquire(6)` then `acquire(4)` → 6+4 == 10, `10 > 10` false →
    /// must NOT block. Kills L29 `>`→`>=` (`10>=10` true → block) and `>`→`==`
    /// (`10==10` true → block).
    // kills byte_budget L29 `>`→`>=` and `>`→`==`
    #[test]
    fn exact_cap_is_admitted() {
        let wait = std::time::Duration::from_millis(100);
        let b = Arc::new(ByteBudget::new(10));
        b.acquire(6); // in_flight = 6
        let (completed, h) = acquire_completes_within(&b, 4, wait);
        assert!(
            completed,
            "6+4 == cap 10 must be admitted (guard is `> cap`, not `>=`/`==`)"
        );
        h.join().unwrap();
        b.release(6);
        b.release(4);
    }

    /// Over cap blocks: `acquire(6)` then `acquire(8)` → 6+8=14, `14 > 10` true →
    /// must block. Kills L29 `>`→`<` (`14 < 10` false → wrongly admitted).
    // kills byte_budget L29 `>`→`<`
    #[test]
    fn over_cap_blocks() {
        let wait = std::time::Duration::from_millis(100);
        let b = Arc::new(ByteBudget::new(10));
        b.acquire(6); // in_flight = 6 (nonzero, so guard is evaluated)
        let (completed, h) = acquire_completes_within(&b, 8, wait);
        assert!(
            !completed,
            "6+8=14 > cap 10 must block; if it completed, the guard admitted an over-cap reservation"
        );
        b.release(6); // unblocks the spawned acquire(8) (idle → admitted alone)
        h.join().unwrap();
        b.release(8);
    }
}
