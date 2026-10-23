-- Work to be done by the one memory service that owns this store. A job that
-- was running when the service stopped is made ready again at the next start:
-- the service fails and restarts clean, and there is no second worker to
-- hand a job to.
CREATE TABLE IF NOT EXISTS jobs (
    kind TEXT NOT NULL,
    job_key TEXT NOT NULL,
    -- ready | running | succeeded | failed. Failed is final until the job is
    -- reopened because its source changed.
    status TEXT NOT NULL,
    finished_at INTEGER,
    last_error TEXT,
    PRIMARY KEY (kind, job_key)
);
