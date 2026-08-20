ALTER TABLE jobs ADD COLUMN callback_published_at timestamptz;

CREATE INDEX jobs_unpublished_callbacks ON jobs (finished_at)
WHERE state IN ('SUCCEEDED', 'FAILED', 'CANCELLED')
  AND callback_queue_url IS NOT NULL
  AND callback_published_at IS NULL;
