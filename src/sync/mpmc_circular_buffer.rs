use std::{
    cmp::max,
    sync::atomic::{AtomicBool, AtomicUsize},
};

use atomic::{Atomic, Ordering};
use std::task::Context;

use super::{
    ref_count::TryDecrement,
    rr_lock::{self, ReadReleaseLock},
};
use std::fmt::Debug;

// A lock-free multi-producer, multi-consumer circular buffer
// Each reader will see each value created exactly once.
// Cloned readers inherit the read location of the reader that was cloned.

pub struct MpmcCircularBuffer<T> {
    buffer: Box<[ReadReleaseLock<T>]>,
    head_lock: AtomicBool,
    head: AtomicUsize,
    tail: AtomicUsize,
}

impl<T> Debug for MpmcCircularBuffer<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MpmcCircularBuffer")
            .field("buffer", &self.buffer)
            .field("head", &self.head)
            .field("tail", &self.tail)
            .finish()
    }
}

impl<T> MpmcCircularBuffer<T>
where
    T: Clone,
{
    pub fn new(capacity: usize) -> (Self, BufferReader) {
        // we require two readers, so that unique slots can be acquired and released
        let capacity = max(2, capacity);
        let mut vec = Vec::with_capacity(capacity);

        for _ in 0..capacity {
            vec.push(ReadReleaseLock::new());
        }

        let this = Self {
            buffer: vec.into_boxed_slice(),
            head: AtomicUsize::new(0),
            head_lock: AtomicBool::new(false),
            tail: AtomicUsize::new(0),
        };

        let reader = BufferReader {
            index: AtomicUsize::new(0),
            state: Atomic::new(ReaderState::Reading),
        };

        this.buffer[0].acquire();

        (this, reader)
    }
}

pub enum TryWrite<T> {
    Pending(T),
    Ready,
}

impl<T> MpmcCircularBuffer<T> {
    pub fn len(&self) -> usize {
        self.buffer.len()
    }

    pub fn try_write(&self, value: T, cx: &Context<'_>) -> TryWrite<T> {
        loop {
            if let Ok(_e) =
                self.head_lock
                    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
            {
                break;
            }
        }

        let head_id = self.head.load(Ordering::SeqCst);
        let next_id = head_id + 1;
        let head_slot = self.get_slot(head_id);

        match head_slot.try_write(value, cx, || {
            self.head.store(next_id, Ordering::SeqCst);
            self.head_lock.store(false, Ordering::Release);
        }) {
            rr_lock::TryWrite::Ready => {
                #[cfg(feature = "logging")]
                log::trace!("Write {} complete: {:?}", head_id, head_slot);

                return TryWrite::Ready;
            }
            rr_lock::TryWrite::Pending(v) => {
                self.head_lock.store(false, Ordering::Release);

                #[cfg(feature = "logging")]
                log::trace!(
                    "Write {} pending, slot is reading: {:?}",
                    head_id,
                    head_slot
                );
                return TryWrite::Pending(v);
            }
        }
    }

    pub fn new_reader(&self) -> BufferReader {
        let index = loop {
            let index = self.head.load(Ordering::SeqCst);
            let slot = self.get_slot(index);

            slot.acquire();
            if index == self.head.load(Ordering::SeqCst) {
                break index;
            }

            slot.decrement();
        };

        BufferReader {
            index: AtomicUsize::new(index),
            state: Atomic::new(ReaderState::Reading),
        }
    }

    pub(in crate::sync::mpmc_circular_buffer) fn get_slot(&self, id: usize) -> &ReadReleaseLock<T> {
        let index = id % self.len();
        &self.buffer[index]
    }
}

#[derive(Debug)]
pub struct BufferReader {
    index: AtomicUsize,
    state: Atomic<ReaderState>,
}

#[derive(Copy, Clone, Debug)]
enum ReaderState {
    Reading,
    Blocked,
}

pub enum TryRead<T> {
    /// A value is ready
    Ready(T),
    /// A value is pending in this slot
    Pending,
}

impl BufferReader {
    pub fn try_read<T>(&self, buffer: &MpmcCircularBuffer<T>, cx: &Context<'_>) -> TryRead<T>
    where
        T: Clone,
    {
        match self.state.load(Ordering::Acquire) {
            ReaderState::Blocked => {
                let index = self.index.load(Ordering::Acquire);
                let head = buffer.head.load(Ordering::Acquire);

                if index >= head {
                    let slot = buffer.get_slot(index);
                    slot.subscribe_write(cx);

                    #[cfg(feature = "logging")]
                    log::trace!(
                        "TryRead {} still blocked on head {}, slot: {:?}",
                        index,
                        head,
                        slot
                    );

                    return TryRead::Pending;
                }

                #[cfg(feature = "logging")]
                log::trace!("TryRead {} un-blocked", index);

                self.state.store(ReaderState::Reading, Ordering::Release);
            }
            ReaderState::Reading => {}
        }

        let index = self.index.load(Ordering::Acquire);
        let slot = buffer.get_slot(index);

        // #[cfg(feature = "logging")]
        // trace!("TryRead {}, slot: {:?}", index, slot);

        match slot.try_read(cx) {
            rr_lock::TryRead::NotLocked => {
                panic!("MPMC slot {} not locked! {:?}", index, slot);
            }
            rr_lock::TryRead::Read(value) => {
                self.advance(index, buffer, cx);
                self.release(index, buffer);

                #[cfg(feature = "logging")]
                log::trace!("Read {} complete, slot: {:?}", index, slot);

                return TryRead::Ready(value);
            }
            rr_lock::TryRead::Pending => {
                #[cfg(feature = "logging")]
                log::trace!("Read {} pending, slot: {:?}", index, slot);
                return TryRead::Pending;
            }
        }
    }

    // To avoid the need for shared Arc references, clone and drop are written as methods instead of using std traits
    pub fn clone<T>(&self, buffer: &MpmcCircularBuffer<T>) -> Self {
        let index = loop {
            let index = self.index.load(Ordering::Acquire);

            let slot = buffer.get_slot(index);
            slot.acquire();

            if index != self.index.load(Ordering::Acquire) {
                slot.decrement();
                continue;
            }

            break index;
        };

        BufferReader {
            index: AtomicUsize::new(index),
            state: Atomic::new(self.state.load(Ordering::Acquire)),
        }
    }

    pub fn drop<T>(&mut self, buffer: &MpmcCircularBuffer<T>) {
        if let ReaderState::Blocked = self.state.load(Ordering::Acquire) {
            let slot = buffer.get_slot(self.index.load(Ordering::Acquire));
            slot.decrement_next();
        } else {
            let id = self.index.load(Ordering::Acquire);
            self.release(id, buffer);
        }
    }

    fn advance<T>(&self, id: usize, buffer: &MpmcCircularBuffer<T>, cx: &Context<'_>) {
        let slot = buffer.get_slot(id);
        let next_id = id + 1;
        let next_slot = buffer.get_slot(next_id);

        loop {
            if let Ok(_e) =
                buffer
                    .head_lock
                    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
            {
                break;
            }
        }

        let head_id = buffer.head.load(Ordering::Acquire);

        if next_id >= head_id {
            next_slot.acquire_next();
            buffer.head_lock.store(false, Ordering::Release);

            // if head_id != buffer.head.load(Ordering::Acquire) {
            //     next_slot.decrement_next();
            //     continue;
            // }

            self.state.store(ReaderState::Blocked, Ordering::Release);
            slot.subscribe_write(cx);
            #[cfg(feature = "logging")]
            log::trace!("Reader blocked at slot {}, head is {}", next_id, head_id);
        } else {
            buffer.get_slot(next_id).acquire();
            buffer.head_lock.store(false, Ordering::Release);
        }
        // }

        // self.index.fetch_add(1, Ordering::AcqRel);
        self.index
            .compare_exchange(id, next_id, Ordering::AcqRel, Ordering::Relaxed)
            .unwrap();
    }

    fn release<T>(&self, id: usize, buffer: &MpmcCircularBuffer<T>) {
        let slot = buffer.get_slot(id);
        match slot.decrement() {
            TryDecrement::Alive => {
                #[cfg(feature = "logging")]
                log::trace!("Read {} decremented", id);
            }
            TryDecrement::Dead => {
                let tail = buffer.tail.load(Ordering::Acquire);
                if id == tail {
                    slot.release()
                        .expect_dead("reader added to tail slot after slot became dead");

                    buffer.tail.store(tail + 1, Ordering::Release);

                    #[cfg(feature = "logging")]
                    log::trace!(
                        "Read {} released, tail incremented from {} to {}",
                        id,
                        tail,
                        tail + 1
                    );
                } else {
                    #[cfg(feature = "logging")]
                    log::trace!("Read {} decremented to 0 (not tail {})", id, tail);
                }
            }
        }
    }
}
