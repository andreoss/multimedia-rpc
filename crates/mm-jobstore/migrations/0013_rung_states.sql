ALTER TABLE jobs ADD COLUMN rung_states bytea;

CREATE OR REPLACE FUNCTION notify_job_event() RETURNS trigger AS $$
BEGIN
    PERFORM pg_notify('mm_job_events', NEW.id::text);
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER jobs_event_notify ON jobs;

CREATE TRIGGER jobs_event_notify
AFTER UPDATE ON jobs
FOR EACH ROW WHEN (NEW.state IS DISTINCT FROM OLD.state
    OR NEW.progress_percent IS DISTINCT FROM OLD.progress_percent
    OR NEW.rung_states IS DISTINCT FROM OLD.rung_states)
EXECUTE FUNCTION notify_job_event();
