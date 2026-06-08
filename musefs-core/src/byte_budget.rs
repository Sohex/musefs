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
        *in_flight = in_flight.saturating_add(n);
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
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    const WAIT: Duration = Duration::from_millis(100);

    /// Spawn `acquire(n)` on a worker and report whether it completed within `wait`
    /// (i.e. did NOT block).
    ///
    /// Deliberately does NOT join the worker: a mutation that turns `acquire` (or
    /// `release`, called by the test afterwards) into a deadlock would make a join
    /// hang forever, which cargo-mutants reports as TIMEOUT (≈20s, fails the gate)
    /// instead of a fast CAUGHT. By polling a flag and leaking any still-blocked
    /// worker (the test harness reaps it at process exit), every such mutation is
    /// caught in ~`wait` by a normal assertion. Tests must never `acquire` on the
    /// main thread for the same reason — only the never-blocking setup `acquire`s
    /// (from idle, which short-circuits the guard) run inline.
    fn completes_within(b: &Arc<ByteBudget>, n: u64, wait: Duration) -> bool {
        let done = Arc::new(AtomicBool::new(false));
        let b2 = Arc::clone(b);
        let done2 = Arc::clone(&done);
        std::thread::spawn(move || {
            b2.acquire(n);
            done2.store(true, Ordering::SeqCst);
        });
        std::thread::sleep(wait);
        done.load(Ordering::SeqCst)
    }

    /// From idle, an item larger than the cap is admitted alone (guarantees
    /// progress). Run on a worker: the `!=`→`==` and `&&`→`||` mutants both turn
    /// this into an infinite wait (`0 == 0 && 1000 > 10` / `0 != 0 || 1000 > 10`
    /// both stay true with nothing to release), so a hang here means CAUGHT.
    // kills byte_budget L29 `!=`→`==` and `&&`→`||`
    #[test]
    fn oversized_item_admitted_when_idle() {
        let b = Arc::new(ByteBudget::new(10));
        assert!(
            completes_within(&b, 1000, WAIT),
            "an oversized item must be admitted alone from idle (else acquire deadlocks)"
        );
    }

    /// A blocked reservation proceeds once enough is released. Pins `release`
    /// (line 37): if it is a no-op the worker never unblocks → not completed.
    // kills byte_budget L37 release→no-op
    #[test]
    fn blocked_acquire_proceeds_after_release() {
        let b = Arc::new(ByteBudget::new(10));
        b.acquire(10); // idle → admitted inline (never blocks under any single mutation)
        let done = Arc::new(AtomicBool::new(false));
        let b2 = Arc::clone(&b);
        let done2 = Arc::clone(&done);
        std::thread::spawn(move || {
            b2.acquire(5); // 10+5 > 10 → blocks until release
            done2.store(true, Ordering::SeqCst);
        });
        std::thread::sleep(WAIT);
        assert!(
            !done.load(Ordering::SeqCst),
            "acquire(5) must block while full"
        );
        b.release(10); // unblocks the worker — unless release was mutated to a no-op
        std::thread::sleep(WAIT);
        assert!(
            done.load(Ordering::SeqCst),
            "acquire(5) must proceed after release(10); a no-op release leaves it blocked"
        );
    }

    /// Accumulation must be additive: from idle, `acquire(4)` then `acquire(7)`
    /// (4+7=11 > cap 10) MUST block. Pins `*in_flight += n` (line 32) and the
    /// acquire body:
    ///   - no-op (`acquire` body → `()`): in_flight stays 0, so the 2nd acquire
    ///     sees 0 and admits 7 → would NOT block → killed.
    ///   - `+=`→`*=`: first acquire makes 0*4 = 0, so in_flight stays 0 → killed.
    // kills byte_budget L25 acquire→no-op and L32 `+=`→`*=`
    #[test]
    fn accumulates_additively_then_blocks() {
        let b = Arc::new(ByteBudget::new(10));
        b.acquire(4); // idle → admitted inline; in_flight = 4
        assert!(
            !completes_within(&b, 7, WAIT),
            "4+7=11 > cap 10 must block; if it completed, accounting is not additive"
        );
    }

    /// Boundary: `in_flight + n == cap` is admitted (guard is strictly `> cap`).
    /// From idle, `acquire(6)` then `acquire(4)` → 6+4 == 10, `10 > 10` false →
    /// must NOT block. Kills L29 `>`→`>=` (`10>=10` true → block) and `>`→`==`
    /// (`10==10` true → block).
    // kills byte_budget L29 `>`→`>=` and `>`→`==`
    #[test]
    fn exact_cap_is_admitted() {
        let b = Arc::new(ByteBudget::new(10));
        b.acquire(6); // idle → admitted inline; in_flight = 6
        assert!(
            completes_within(&b, 4, WAIT),
            "6+4 == cap 10 must be admitted (guard is `> cap`, not `>=`/`==`)"
        );
    }

    /// Over cap blocks: `acquire(6)` then `acquire(8)` → 6+8=14, `14 > 10` true →
    /// must block. Kills L29 `>`→`<` (`14 < 10` false → wrongly admitted).
    // kills byte_budget L29 `>`→`<`
    #[test]
    fn over_cap_blocks() {
        let b = Arc::new(ByteBudget::new(10));
        b.acquire(6); // idle → admitted inline; in_flight = 6
        assert!(
            !completes_within(&b, 8, WAIT),
            "6+8=14 > cap 10 must block; if it completed, the guard admitted an over-cap reservation"
        );
    }
}
