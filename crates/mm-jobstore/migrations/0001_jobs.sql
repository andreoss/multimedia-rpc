CREATE TABLE jobs (
    id uuid PRIMARY KEY,
    user_id text NOT NULL,
    request_id text NOT NULL,
    spec bytea NOT NULL,
    state text NOT NULL DEFAULT 'PENDING' CHECK (
        state IN ('PENDING', 'RUNNING', 'SUCCEEDED', 'FAILED', 'CANCELLED')
    ),
    progress_percent smallint NOT NULL DEFAULT 0 CHECK (
        progress_percent BETWEEN 0 AND 100
    ),
    error_code text,
    error_message text,
    result bytea,
    created_at timestamptz NOT NULL DEFAULT now(),
    started_at timestamptz,
    finished_at timestamptz,
    attempts integer NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    next_attempt_at timestamptz,
    deadline_at timestamptz,
    claimed_by text,
    lease_expires_at timestamptz,
    cancel_requested boolean NOT NULL DEFAULT false,
    CONSTRAINT jobs_request_id_unique UNIQUE (request_id)
);

CREATE INDEX jobs_owner_keyset_idx ON jobs (user_id, created_at DESC, id DESC);

CREATE INDEX jobs_claimable_idx ON jobs (created_at)
    WHERE state = 'PENDING';

CREATE INDEX jobs_lease_idx ON jobs (lease_expires_at)
    WHERE state = 'RUNNING';
