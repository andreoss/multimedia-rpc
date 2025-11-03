CREATE FUNCTION notify_job_ready() RETURNS trigger AS $$
BEGIN
    PERFORM pg_notify('mm_job_ready', NEW.id::text);
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER jobs_insert_notify
AFTER INSERT ON jobs
FOR EACH ROW WHEN (NEW.state = 'PENDING')
EXECUTE FUNCTION notify_job_ready();

CREATE TRIGGER jobs_requeue_notify
AFTER UPDATE OF state ON jobs
FOR EACH ROW WHEN (NEW.state = 'PENDING' AND OLD.state <> 'PENDING')
EXECUTE FUNCTION notify_job_ready();
