ALTER TABLE jobs ADD COLUMN trace_id text;

CREATE INDEX jobs_trace_idx ON jobs (trace_id) WHERE trace_id IS NOT NULL;