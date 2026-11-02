//! A single-writer, double-buffered sequence lock over a fixed array of 64-bit words
//! (dsync#136): how the follower thread publishes its clock model to any number of real-time
//! readers without a lock — and without a reader ever waiting for the writer.
//!
//! Two slots, each with its own sequence. The writer fills the INACTIVE slot (its sequence odd
//! while it writes, even again after, Release) and then makes it the active one (Release). A
//! reader loads the active index (Acquire), copies that slot's words between two reads of its
//! sequence, and retries only if that slot was written meanwhile — which needs the writer to have
//! completed a whole write AND started the next one during the copy — or if the active index
//! changed since the reader loaded it (a slot rewritten but not yet published is never read). So a reader that preempts
//! the writer mid-write (an RT audio thread at SCHED_FIFO on the writer's core) never spins on
//! it: it reads the other, complete slot. The words are atomics, so an overlapping copy is never
//! undefined behaviour — it is only discarded. No allocation, no lock, no OS call.
//!
//! Single writer: [`SeqWriter`] is the only way to write and is not `Clone`.

use std::sync::atomic::{fence, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

#[derive(Debug)]
struct Slot<const N: usize> {
    seq: AtomicU64,
    words: [AtomicU64; N],
}

impl<const N: usize> Slot<N> {
    fn new(init: [u64; N]) -> Self {
        Slot {
            seq: AtomicU64::new(0),
            words: init.map(AtomicU64::new),
        }
    }

    /// A consistent copy, or `None` when a write overlapped.
    fn try_read(&self) -> Option<[u64; N]> {
        let s1 = self.seq.load(Ordering::Acquire);
        if s1 & 1 == 1 {
            return None;
        }
        let mut out = [0u64; N];
        for (o, w) in out.iter_mut().zip(self.words.iter()) {
            *o = w.load(Ordering::Relaxed);
        }
        fence(Ordering::Acquire);
        (self.seq.load(Ordering::Relaxed) == s1).then_some(out)
    }
}

/// The shared slots.
#[derive(Debug)]
pub struct SeqLock<const N: usize> {
    active: AtomicUsize,
    slots: [Slot<N>; 2],
    writes: AtomicU64,
}

impl<const N: usize> SeqLock<N> {
    /// A lock holding `init`, and the one writer for it.
    pub fn new(init: [u64; N]) -> (Arc<Self>, SeqWriter<N>) {
        let lock = Arc::new(SeqLock {
            active: AtomicUsize::new(0),
            slots: [Slot::new(init), Slot::new(init)],
            writes: AtomicU64::new(0),
        });
        let writer = SeqWriter {
            lock: Arc::clone(&lock),
        };
        (lock, writer)
    }

    /// A consistent copy of the last completed write (never a mix of two writes).
    pub fn read(&self) -> [u64; N] {
        loop {
            let i = self.active.load(Ordering::Acquire);
            if let Some(words) = self.try_read_at(i) {
                return words;
            }
            std::hint::spin_loop();
        }
    }

    /// One read attempt of slot `i`, the active index this reader loaded (maybe long ago).
    ///
    /// The copy counts only if `i` is still the active index after it. Without that check a
    /// reader that stalled after loading `i` — while the writer published the other slot and
    /// then completely rewrote slot `i` without publishing it yet — returns that unpublished
    /// write, and its next read returns the published, older one: time goes back (dsync#150 CI).
    /// Retrying here never waits for a write in progress: the index only changes when a write has
    /// completed. Ordering: reading a word the writer stored after its Release fence, then the
    /// Acquire fence in [`Slot::try_read`], makes every earlier publication of `active` visible.
    fn try_read_at(&self, i: usize) -> Option<[u64; N]> {
        let words = self.slots[i & 1].try_read()?;
        (self.active.load(Ordering::Acquire) == i).then_some(words)
    }

    /// How many writes completed.
    pub fn writes(&self) -> u64 {
        self.writes.load(Ordering::Acquire)
    }
}

/// The only writer of a [`SeqLock`].
#[derive(Debug)]
pub struct SeqWriter<const N: usize> {
    lock: Arc<SeqLock<N>>,
}

impl<const N: usize> SeqWriter<N> {
    /// Publish `words` (every reader sees all of them or none).
    pub fn write(&mut self, words: [u64; N]) {
        let lock = &*self.lock;
        let next = (lock.active.load(Ordering::Relaxed) + 1) & 1;
        let slot = &lock.slots[next];
        let s = slot.seq.load(Ordering::Relaxed);
        slot.seq.store(s.wrapping_add(1), Ordering::Relaxed);
        fence(Ordering::Release);
        for (w, v) in slot.words.iter().zip(words) {
            w.store(v, Ordering::Relaxed);
        }
        slot.seq.store(s.wrapping_add(2), Ordering::Release);
        lock.active.store(next, Ordering::Release);
        lock.writes.fetch_add(1, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn a_read_returns_the_last_write() {
        let (lock, mut w) = SeqLock::new([1, 2, 3]);
        assert_eq!(lock.read(), [1, 2, 3]);
        w.write([4, 5, 6]);
        assert_eq!(lock.read(), [4, 5, 6]);
        w.write([7, 8, 9]);
        assert_eq!(lock.read(), [7, 8, 9]);
        assert_eq!(lock.writes(), 2);
    }

    /// A reader that runs while a write is half done (the writer preempted between its stores)
    /// reads the other, complete slot at once: it never waits for the writer.
    #[test]
    fn a_write_in_progress_never_blocks_a_reader() {
        let (lock, mut w) = SeqLock::new([0u64; 2]);
        w.write([1, 1]);
        // Freeze a write half way: the inactive slot's sequence odd, one word stored.
        let next = (lock.active.load(Ordering::Relaxed) + 1) & 1;
        lock.slots[next].seq.fetch_add(1, Ordering::Relaxed);
        lock.slots[next].words[0].store(2, Ordering::Relaxed);
        assert_eq!(lock.read(), [1, 1], "the complete slot, no spin");
    }

    /// dsync#150 CI (Coverage, run 41468401534: "a read went back in time"): a reader that
    /// loaded the active index, then stalled while the writer published the other slot and
    /// completely rewrote this one (not yet published), must not return that unpublished value:
    /// its next read returns the published, older one, so the reader would go back in time.
    #[test]
    fn a_stale_index_never_returns_a_complete_but_unpublished_write() {
        let (lock, mut w) = SeqLock::new([0u64; 2]);
        w.write([1, 1]);
        // A reader loads this active index, then stalls.
        let loaded = lock.active.load(Ordering::Relaxed);
        w.write([2, 2]); // the other slot, now active
                         // The writer completes the reader's slot with 3 and is preempted before it publishes it.
        let slot = &lock.slots[loaded];
        let s = slot.seq.load(Ordering::Relaxed);
        slot.seq.store(s + 2, Ordering::Relaxed);
        for word in &slot.words {
            word.store(3, Ordering::Relaxed);
        }
        assert_eq!(
            lock.try_read_at(loaded),
            None,
            "the index changed since it was loaded: retry"
        );
        assert_eq!(lock.read(), [2, 2], "the published write");
    }

    /// Every write keeps all words equal; a torn read would see two different values.
    #[test]
    fn concurrent_readers_never_see_a_torn_write() {
        let (lock, mut w) = SeqLock::new([0u64; 8]);
        let stop = Arc::new(AtomicBool::new(false));
        let readers: Vec<_> = (0..3)
            .map(|_| {
                let lock = Arc::clone(&lock);
                let stop = Arc::clone(&stop);
                std::thread::spawn(move || {
                    let mut reads = 0u64;
                    let mut last = 0u64;
                    while !stop.load(Ordering::Relaxed) {
                        let words = lock.read();
                        assert!(
                            words.iter().all(|&x| x == words[0]),
                            "torn read: {:?}",
                            words
                        );
                        assert!(words[0] >= last, "a read went back in time");
                        last = words[0];
                        reads += 1;
                    }
                    reads
                })
            })
            .collect();
        for i in 1..=200_000u64 {
            w.write([i; 8]);
        }
        stop.store(true, Ordering::Relaxed);
        for r in readers {
            assert!(r.join().unwrap() > 0);
        }
        assert_eq!(lock.read(), [200_000; 8]);
    }
}
