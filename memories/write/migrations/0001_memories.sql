-- What one finished session was worth remembering, as the extraction pass left
-- it. One row per session: a second pass over the same session replaces it
-- rather than accumulating.
CREATE TABLE stage1_outputs (
    session_id TEXT PRIMARY KEY,
    -- When the session itself last changed: the time of its last record.
    source_updated_at INTEGER NOT NULL,
    raw_memory TEXT NOT NULL,
    session_summary TEXT NOT NULL,
    session_slug TEXT,
    generated_at INTEGER NOT NULL,
    -- How many sessions have cited this, and when one last did. NULL means
    -- never cited, which is not the same as cited zero times recently: a fresh
    -- memory has had no chance yet, and selection treats the two differently.
    usage_count INTEGER,
    last_usage INTEGER,
    -- Whether a successful consolidation has consumed this row, and the
    -- version of it that was consumed. An extraction that rewrites the row
    -- leaves this alone.
    selected_for_consolidation INTEGER NOT NULL DEFAULT 0,
    selected_source_updated_at INTEGER
);

CREATE INDEX idx_stage1_outputs_source_updated_at
    ON stage1_outputs(source_updated_at DESC, session_id DESC);

-- Which sessions have used which memory. One row per pair, so a session that
-- cites the same memory in ten turns has used it once: the count decides what
-- survives consolidation, and a memory that could be kept alive by being cited
-- repeatedly in one conversation would be kept alive by itself.
CREATE TABLE usage_by_session (
    -- The row in stage1_outputs that was used.
    memory_session TEXT NOT NULL,
    -- The session that used it.
    citing_session TEXT NOT NULL,
    first_used INTEGER NOT NULL,
    PRIMARY KEY (memory_session, citing_session)
);
