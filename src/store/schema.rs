pub(super) const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS commissions (
    id TEXT PRIMARY KEY,
    goal TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('proposed', 'active', 'verified_complete')),
    revision INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    accepted_at INTEGER,
    completed_at INTEGER,
    artifact_revision TEXT
);

CREATE TABLE IF NOT EXISTS criteria (
    commission_id TEXT NOT NULL REFERENCES commissions(id),
    criterion_id TEXT NOT NULL,
    position INTEGER NOT NULL,
    description TEXT NOT NULL,
    verifier_kind TEXT NOT NULL,
    expected TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('pending', 'passed', 'failed')),
    PRIMARY KEY (commission_id, criterion_id)
);

CREATE TABLE IF NOT EXISTS authority_scopes (
    commission_id TEXT NOT NULL REFERENCES commissions(id),
    scope_type TEXT NOT NULL CHECK (scope_type IN ('repository', 'path', 'action', 'destination', 'effect')),
    position INTEGER NOT NULL,
    value TEXT NOT NULL,
    PRIMARY KEY (commission_id, scope_type, position)
);

CREATE TABLE IF NOT EXISTS resource_ceilings (
    commission_id TEXT PRIMARY KEY REFERENCES commissions(id),
    max_attempts INTEGER NOT NULL,
    max_elapsed_seconds INTEGER NOT NULL,
    max_worker_concurrency INTEGER NOT NULL,
    max_storage_bytes INTEGER NOT NULL,
    max_model_spend_cents INTEGER NOT NULL,
    max_paid_service_spend_cents INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS known_uncertainties (
    commission_id TEXT NOT NULL REFERENCES commissions(id),
    position INTEGER NOT NULL,
    description TEXT NOT NULL,
    PRIMARY KEY (commission_id, position)
);

CREATE TABLE IF NOT EXISTS assignments (
    id TEXT PRIMARY KEY,
    commission_id TEXT NOT NULL REFERENCES commissions(id),
    plan_revision INTEGER NOT NULL,
    status TEXT NOT NULL CHECK (status IN (
        'ready', 'running', 'accepted', 'verification_failed', 'resource_blocked'
    )),
    created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS attempts (
    id TEXT PRIMARY KEY,
    assignment_id TEXT NOT NULL REFERENCES assignments(id),
    worker_configuration TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('running', 'succeeded')),
    started_at INTEGER NOT NULL,
    completed_at INTEGER
);

CREATE TABLE IF NOT EXISTS results (
    id TEXT PRIMARY KEY,
    attempt_id TEXT NOT NULL REFERENCES attempts(id),
    output TEXT NOT NULL,
    artifact_revision TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('candidate', 'accepted')),
    created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS evidence (
    id TEXT PRIMARY KEY,
    commission_id TEXT NOT NULL REFERENCES commissions(id),
    criterion_id TEXT NOT NULL,
    result_id TEXT NOT NULL REFERENCES results(id),
    mandate_revision INTEGER NOT NULL,
    artifact_revision TEXT NOT NULL,
    verifier_kind TEXT NOT NULL CHECK (verifier_kind IN ('exact_match')),
    outcome TEXT NOT NULL CHECK (outcome IN ('passed', 'failed')),
    observed TEXT NOT NULL,
    expected TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    FOREIGN KEY (commission_id, criterion_id) REFERENCES criteria(commission_id, criterion_id)
);

CREATE TABLE IF NOT EXISTS completion_briefings (
    commission_id TEXT PRIMARY KEY REFERENCES commissions(id),
    summary TEXT NOT NULL,
    artifact_revision TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS blockers (
    id TEXT PRIMARY KEY,
    commission_id TEXT NOT NULL REFERENCES commissions(id),
    assignment_id TEXT NOT NULL UNIQUE REFERENCES assignments(id),
    code TEXT NOT NULL,
    requirement TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS events (
    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
    commission_id TEXT NOT NULL REFERENCES commissions(id),
    event_type TEXT NOT NULL CHECK (event_type IN (
        'commission_proposed', 'commission_accepted', 'assignment_ready',
        'attempt_started', 'result_submitted', 'evidence_recorded',
        'commission_verified_complete', 'assignment_blocked'
    )),
    commission_revision INTEGER NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS idempotency_keys (
    key TEXT PRIMARY KEY,
    request_hash TEXT NOT NULL,
    response_json TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
"#;
