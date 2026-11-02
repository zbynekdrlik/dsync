//! A single-writer sequence lock over a fixed array of 64-bit words (dantesync#136): how the
//! follower thread publishes its clock model to any number of real-time readers without a lock.
//!
//! The writer makes the sequence ODD, stores the words, and makes it EVEN again (Release). A reader
//! loads the sequence (Acquire), the words, then the sequence again: an odd or changed sequence
//! means a write overlapped, and it reads again. The words are atomics, so an overlapping read is
//! never undefined behaviour — it is only discarded. The writer runs at the PTP window rate
//! (~2 Hz), so a reader retries at most once in practice; it never blocks, allocates or calls the
//! OS.
//!
//! Single writer: [`SeqWriter`] is the only way to write and is not `Clone`.

use std::sync::atomic::{fence, AtomicU64, Ordering};
use std::sync::Arc;

/// The shared words and their sequence.
#[derive(Debug)]
pub struct SeqLock<const N: usize> {
    seq: AtomicU64,
    words: [AtomicU64; N],
}

impl<const N: usize> SeqLock<N> {
    /// A lock holding `init`, and the one writer for it.
    pub fn new(init: [u64; N]) -> (Arc<Self>, SeqWriter<N>) {
        let lock = Arc::new(SeqLock {
            seq: AtomicU64::new(0),
            words: init.map(AtomicU64::new),
        });
        let writer = SeqWriter {
            lock: Arc::clone(&lock),
        };
        (lock, writer)
    }

    /// A consistent copy of the words (never a mix of two writes).
    pub fn read(&self) -> [u64; N] {
        loop {
            let s1 = self.seq.load(Ordering::Acquire);
            if s1 & 1 == 1 {
                std::hint::spin_loop();
                continue;
            }
            let mut out = [0u64; N];
            for (o, w) in out.iter_mut().zip(self.words.iter()) {
                *o = w.load(Ordering::Relaxed);
            }
            fence(Ordering::Acquire);
            let s2 = self.seq.load(Ordering::Relaxed);
            if s1 == s2 {
                return out;
            }
            std::hint::spin_loop();
        }
    }

    /// How many writes completed.
    pub fn writes(&self) -> u64 {
        self.seq.load(Ordering::Acquire) / 2
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
        let s = lock.seq.load(Ordering::Relaxed);
        lock.seq.store(s.wrapping_add(1), Ordering::Relaxed);
        fence(Ordering::Release);
        for (w, v) in lock.words.iter().zip(words) {
            w.store(v, Ordering::Relaxed);
        }
        lock.seq.store(s.wrapping_add(2), Ordering::Release);
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
        assert_eq!(lock.writes(), 1);
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
