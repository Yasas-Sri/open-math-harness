-- The memory files as the last successful consolidation left them, keyed by
-- path relative to the workspace. The diff against these rows is the next
-- consolidation's prompt, and `MEMORY.md` here is what readers are given.
CREATE TABLE IF NOT EXISTS baseline_files (
    path TEXT PRIMARY KEY,
    content TEXT NOT NULL
);

-- One row once a baseline has been recorded. `published_at` is when the
-- baselined `MEMORY.md` last changed (unix milliseconds, 0 for never).
CREATE TABLE IF NOT EXISTS baseline (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    published_at INTEGER NOT NULL
);
