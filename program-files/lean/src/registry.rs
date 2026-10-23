//! The frozen files `register` keeps, by `[run, problem]`: in memory only,
//! at most [`MAX_REGISTRATIONS`], the least recently used dropped first. A
//! registration is never replaced (the service refuses another text for a
//! registered key), so a run's standard never changes under it; one that was
//! dropped, or lost with the process, is registered again by the discovery
//! service when a call fails with `not_registered`.

use std::collections::HashMap;

use nucleus_lean_interface::MAX_REGISTRATIONS;

struct Entry {
    frozen: String,
    /// The pristine type `register` computed for `frozen`.
    pristine: String,
    /// When it was last registered or read ([`Registry::clock`]).
    used: u64,
}

#[derive(Default)]
pub struct Registry {
    entries: HashMap<(String, String), Entry>,
    /// Counts registrations and reads, to order entries by last use.
    clock: u64,
}

impl Registry {
    /// The frozen file and pristine type registered for `[run, problem]`,
    /// now the most recently used.
    pub fn get(&mut self, run: &str, problem: &str) -> Option<(&str, &str)> {
        self.clock += 1;
        let entry = self.entries.get_mut(&(run.to_owned(), problem.to_owned()))?;
        entry.used = self.clock;
        Some((&entry.frozen, &entry.pristine))
    }

    /// Keep `frozen` and its pristine type for `[run, problem]`, which the
    /// caller found unregistered, dropping the least recently used entry
    /// first when the registry is full.
    pub fn insert(&mut self, run: String, problem: String, frozen: String, pristine: String) {
        if self.entries.len() >= MAX_REGISTRATIONS {
            let oldest = self.entries.iter().min_by_key(|(_, e)| e.used).map(|(key, _)| key.clone());
            if let Some(oldest) = oldest {
                self.entries.remove(&oldest);
            }
        }
        self.clock += 1;
        self.entries.insert((run, problem), Entry { frozen, pristine, used: self.clock });
    }

    /// Drops every registration of `run` (the interface's `forget`).
    pub fn forget(&mut self, run: &str) {
        self.entries.retain(|(registered, _), _| registered != run);
    }
}
