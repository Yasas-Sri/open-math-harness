//! Provider slots: at most `--max-inflight` HTTP attempts at once, `complete`
//! and `start` alike, handed out first come, first served.
//!
//! A slot is held for one attempt, from the moment the request is about to
//! be sent until its transport is released, and given back before a retry's
//! backoff: a call that is waiting to try again is not using the provider.
//! A caller waiting for a slot is not sent anything; its own `waiting` check
//! (cancellation, deadline, and for a stream the queue lease) runs at least
//! every [`POLL`].
//!
//! [`Table`] is the pure bookkeeping and [`Slots`] shares it between the
//! threads that make provider calls.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use nucleus_protocol::Error;

/// How often a waiting caller runs its `waiting` check.
pub const POLL: Duration = Duration::from_millis(50);

/// Who holds a slot and who waits for one, in arrival order.
#[derive(Debug)]
pub struct Table {
    max: usize,
    held: usize,
    waiting: VecDeque<u64>,
    next_ticket: u64,
}

impl Table {
    /// `max` is clamped to at least one: a table that can never hand out a
    /// slot would hold every call forever.
    pub fn new(max: usize) -> Self {
        Self { max: max.max(1), held: 0, waiting: VecDeque::new(), next_ticket: 0 }
    }

    /// Joins the end of the line.
    pub fn enqueue(&mut self) -> u64 {
        self.next_ticket += 1;
        self.waiting.push_back(self.next_ticket);
        self.next_ticket
    }

    /// Takes a slot for `ticket` if one is free and nobody is ahead of it.
    pub fn take(&mut self, ticket: u64) -> bool {
        if self.held < self.max && self.waiting.front() == Some(&ticket) {
            self.waiting.pop_front();
            self.held += 1;
            true
        } else {
            false
        }
    }

    /// Gives up waiting. Unknown tickets change nothing.
    pub fn leave(&mut self, ticket: u64) {
        self.waiting.retain(|waiting| *waiting != ticket);
    }

    /// A holder is done with its slot.
    pub fn release(&mut self) {
        self.held = self.held.saturating_sub(1);
    }

    #[cfg(test)]
    pub fn held(&self) -> usize {
        self.held
    }

    #[cfg(test)]
    pub fn waiting(&self) -> usize {
        self.waiting.len()
    }
}

/// The table shared by every thread that calls the provider.
#[derive(Clone)]
pub struct Slots {
    table: Arc<Mutex<Table>>,
    changed: Arc<Condvar>,
}

impl Slots {
    pub fn new(max: usize) -> Self {
        Self { table: Arc::new(Mutex::new(Table::new(max))), changed: Arc::default() }
    }

    fn lock(&self) -> MutexGuard<'_, Table> {
        self.table.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Waits for a slot in arrival order. When one is not free at once,
    /// `waiting` runs before every wait and ends the wait with its error.
    /// The table's lock is never held while `waiting` runs.
    pub fn acquire(&self, mut waiting: impl FnMut() -> Result<(), Error>) -> Result<Slot, Error> {
        let mut table = self.lock();
        let ticket = table.enqueue();
        loop {
            if table.take(ticket) {
                return Ok(Slot { slots: self.clone() });
            }
            drop(table);
            if let Err(error) = waiting() {
                self.lock().leave(ticket);
                // Someone behind may now be first.
                self.changed.notify_all();
                return Err(error);
            }
            table = self.lock();
            if table.take(ticket) {
                return Ok(Slot { slots: self.clone() });
            }
            table = self.changed.wait_timeout(table, POLL).unwrap_or_else(PoisonError::into_inner).0;
        }
    }

    /// Slots held and callers waiting.
    #[cfg(test)]
    pub fn counts(&self) -> (usize, usize) {
        let table = self.lock();
        (table.held(), table.waiting())
    }
}

/// One held slot. Given back when dropped.
pub struct Slot {
    slots: Slots,
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.slots.lock().release();
        self.slots.changed.notify_all();
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use nucleus_protocol::ErrorCode;

    use super::*;

    #[test]
    fn slots_are_taken_in_arrival_order_and_never_beyond_the_bound() {
        let mut table = Table::new(1);
        let (first, second, third) = (table.enqueue(), table.enqueue(), table.enqueue());
        assert!(!table.take(second), "nobody jumps the line");
        assert!(table.take(first));
        assert!(!table.take(second), "one slot, held");
        table.release();
        table.leave(second);
        assert!(table.take(third), "a caller that left is skipped");
        assert_eq!((table.held(), table.waiting()), (1, 0));
        assert_eq!(Table::new(0).max, 1, "zero slots would mean no service at all");
    }

    #[test]
    fn a_waiting_caller_gets_the_slot_when_it_is_given_back_and_can_give_up_waiting() {
        let slots = Slots::new(1);
        let held = slots.acquire(|| Ok(())).unwrap();
        let waiter = slots.clone();
        let (checked, checks) = std::sync::mpsc::channel();
        let second = std::thread::spawn(move || {
            waiter.acquire(|| {
                let _ = checked.send(());
                Ok(())
            })
        });
        checks.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(slots.counts(), (1, 1), "the second caller waits and runs its check");
        drop(held);
        let slot = second.join().unwrap().unwrap();
        assert_eq!(slots.counts(), (1, 0));

        let refused = slots.acquire(|| Err(Error::new(ErrorCode::Cancelled, "gone"))).err().unwrap();
        assert_eq!(refused.code, ErrorCode::Cancelled);
        assert_eq!(slots.counts(), (1, 0), "a caller that gave up no longer waits");
        drop(slot);
        assert_eq!(slots.counts(), (0, 0));
    }
}
