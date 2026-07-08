ALTER TABLE jobs
    ADD COLUMN requester_id text,
    ADD COLUMN requester_type text
        CHECK (
            requester_type IS NULL OR requester_type IN (
                'CHAT_MESSAGE',
                'CANVAS_EXECUTION_TASK',
                'SERIES_EPISODE',
                'STUDIO'
            )
        ),
    ADD COLUMN input_sequence bytea;

ALTER TABLE jobs
    ADD CONSTRAINT jobs_requester_pair CHECK (
        (requester_id IS NULL) = (requester_type IS NULL)
    );

CREATE INDEX jobs_requester_idx ON jobs (requester_id)
    WHERE requester_id IS NOT NULL;