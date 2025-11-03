CREATE FUNCTION notify_job_cancel() RETURNS trigger AS $$
BEGIN
    PERFORM pg_notify('mm_job_cancel', NEW.id::text);
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER jobs_cancel_notify
AFTER UPDATE OF cancel_requested ON jobs
FOR EACH ROW WHEN (NEW.cancel_requested AND NOT OLD.cancel_requested)
EXECUTE FUNCTION notify_job_cancel();
