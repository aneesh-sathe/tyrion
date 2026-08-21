use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{backup::Backup, Connection, OptionalExtension};

use crate::TyrionError;

pub(super) const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS commissions (
    id TEXT PRIMARY KEY,
    goal TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('proposed', 'active', 'verified_complete')),
    revision INTEGER NOT NULL,
    control_revision INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,
    accepted_at INTEGER,
    completed_at INTEGER,
    artifact_revision TEXT,
    execution_json TEXT NOT NULL DEFAULT '{"kind":"deterministic"}'
);

CREATE TABLE IF NOT EXISTS criteria (
    commission_id TEXT NOT NULL REFERENCES commissions(id),
    criterion_id TEXT NOT NULL,
    position INTEGER NOT NULL,
    description TEXT NOT NULL,
    required_evidence TEXT NOT NULL,
    verifier_type TEXT NOT NULL CHECK (verifier_type IN ('deterministic', 'model', 'principal')),
    verification_depth TEXT NOT NULL CHECK (verification_depth IN ('standard', 'independent')),
    verifier_configuration TEXT NOT NULL,
    verification_environment TEXT NOT NULL,
    verifier_kind TEXT NOT NULL CHECK (verifier_kind IN ('exact_match', 'command', 'prompt')),
    expected TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('uncertain', 'passed', 'failed')),
    PRIMARY KEY (commission_id, criterion_id)
);

CREATE TABLE IF NOT EXISTS criterion_versions (
    commission_id TEXT NOT NULL REFERENCES commissions(id),
    mandate_revision INTEGER NOT NULL,
    criterion_id TEXT NOT NULL,
    position INTEGER NOT NULL,
    description TEXT NOT NULL,
    required_evidence TEXT NOT NULL,
    verifier_type TEXT NOT NULL CHECK (verifier_type IN ('deterministic', 'model', 'principal')),
    verification_depth TEXT NOT NULL CHECK (verification_depth IN ('standard', 'independent')),
    verifier_configuration TEXT NOT NULL,
    verification_environment TEXT NOT NULL,
    verifier_kind TEXT NOT NULL CHECK (verifier_kind IN ('exact_match', 'command', 'prompt')),
    expected TEXT NOT NULL,
    PRIMARY KEY (commission_id, mandate_revision, criterion_id)
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
        'ready', 'running', 'accepted', 'verification_pending', 'verification_failed', 'resource_blocked'
    )),
    created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS attempts (
    id TEXT PRIMARY KEY,
    assignment_id TEXT NOT NULL REFERENCES assignments(id),
    worker_configuration TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('running', 'succeeded', 'failed')),
    started_at INTEGER NOT NULL,
    completed_at INTEGER
);

CREATE TABLE IF NOT EXISTS worker_leases (
    id TEXT PRIMARY KEY,
    attempt_id TEXT NOT NULL UNIQUE REFERENCES attempts(id),
    issued_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    released_at INTEGER,
    status TEXT NOT NULL CHECK (status IN ('active', 'released', 'revoked', 'expired'))
);

CREATE TABLE IF NOT EXISTS results (
    id TEXT PRIMARY KEY,
    attempt_id TEXT NOT NULL REFERENCES attempts(id),
    output TEXT NOT NULL,
    artifact_revision TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('candidate', 'accepted', 'superseded')),
    created_at INTEGER NOT NULL,
    mandate_revision INTEGER,
    base_revision TEXT,
    candidate_commits_json TEXT NOT NULL DEFAULT '[]',
    changed_paths_json TEXT NOT NULL DEFAULT '[]',
    artifacts_json TEXT NOT NULL DEFAULT '[]',
    verification_outcomes_json TEXT NOT NULL DEFAULT '[]',
    known_effects_json TEXT NOT NULL DEFAULT '[]',
    integrated_artifact_revision TEXT
);

CREATE TABLE IF NOT EXISTS evidence (
    id TEXT PRIMARY KEY,
    commission_id TEXT NOT NULL REFERENCES commissions(id),
    criterion_id TEXT NOT NULL,
    result_id TEXT NOT NULL REFERENCES results(id),
    mandate_revision INTEGER NOT NULL,
    artifact_revision TEXT NOT NULL,
    evidence_type TEXT NOT NULL,
    verifier_type TEXT NOT NULL CHECK (verifier_type IN ('deterministic', 'model', 'principal')),
    scope TEXT NOT NULL CHECK (scope IN ('candidate', 'integrated', 'external')),
    verification_attempt_id TEXT NOT NULL,
    verifier_identity TEXT NOT NULL,
    verifier_configuration TEXT NOT NULL,
    verifier_kind TEXT NOT NULL CHECK (verifier_kind IN ('exact_match', 'command', 'prompt')),
    procedure_json TEXT NOT NULL,
    environment TEXT NOT NULL,
    outcome TEXT NOT NULL CHECK (outcome IN ('passed', 'failed', 'uncertain')),
    observed TEXT NOT NULL,
    expected TEXT NOT NULL,
    material_contradiction INTEGER NOT NULL CHECK (material_contradiction IN (0, 1)),
    defect TEXT CHECK (defect IN ('result', 'verifier', 'environment', 'criterion')),
    producer_attempt_id TEXT,
    created_at INTEGER NOT NULL,
    UNIQUE (commission_id, criterion_id, evidence_type, verification_attempt_id),
    FOREIGN KEY (commission_id, criterion_id) REFERENCES criteria(commission_id, criterion_id)
);

CREATE TRIGGER IF NOT EXISTS evidence_is_immutable_update
BEFORE UPDATE ON evidence
BEGIN
    SELECT RAISE(ABORT, 'Evidence is immutable');
END;

CREATE TRIGGER IF NOT EXISTS evidence_is_immutable_delete
BEFORE DELETE ON evidence
BEGIN
    SELECT RAISE(ABORT, 'Evidence is immutable');
END;

CREATE TABLE IF NOT EXISTS verification_gates (
    commission_id TEXT NOT NULL REFERENCES commissions(id),
    criterion_id TEXT NOT NULL,
    mandate_revision INTEGER NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('open', 'closed')),
    opened_at INTEGER NOT NULL,
    closed_at INTEGER,
    PRIMARY KEY (commission_id, criterion_id, mandate_revision),
    FOREIGN KEY (commission_id, criterion_id) REFERENCES criteria(commission_id, criterion_id)
);

CREATE TABLE IF NOT EXISTS verification_recoveries (
    id TEXT PRIMARY KEY,
    commission_id TEXT NOT NULL REFERENCES commissions(id),
    criterion_id TEXT NOT NULL,
    result_id TEXT NOT NULL REFERENCES results(id),
    source_evidence_id TEXT NOT NULL UNIQUE REFERENCES evidence(id),
    mandate_revision INTEGER NOT NULL,
    action TEXT NOT NULL CHECK (action IN ('rework', 'retry', 'reroute', 'escalate', 'block')),
    status TEXT NOT NULL CHECK (status IN ('pending', 'scheduled', 'attention_required', 'blocked', 'resolved')),
    requirement TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    resolved_at INTEGER,
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

CREATE TABLE IF NOT EXISTS attachment_launch_tokens (
    token_hash TEXT PRIMARY KEY,
    expected_harness TEXT NOT NULL,
    expected_adapter_identity TEXT NOT NULL,
    expected_adapter_version TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    consumed_at INTEGER
);

CREATE TABLE IF NOT EXISTS attachments (
    id TEXT PRIMARY KEY,
    session_token_hash TEXT NOT NULL UNIQUE,
    harness TEXT NOT NULL,
    adapter_identity TEXT NOT NULL,
    adapter_version TEXT NOT NULL,
    protocol_version INTEGER NOT NULL,
    native_session_id TEXT NOT NULL,
    mode TEXT NOT NULL CHECK (mode IN ('full', 'limited', 'observer')),
    capabilities_json TEXT NOT NULL,
    missing_capabilities_json TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS commission_attachments (
    commission_id TEXT NOT NULL REFERENCES commissions(id),
    attachment_id TEXT NOT NULL REFERENCES attachments(id),
    role TEXT NOT NULL CHECK (role IN ('active', 'observer')),
    joined_at INTEGER NOT NULL,
    PRIMARY KEY (commission_id, attachment_id)
);

CREATE UNIQUE INDEX IF NOT EXISTS one_active_attachment_per_commission
ON commission_attachments (commission_id) WHERE role = 'active';

CREATE TABLE IF NOT EXISTS events (
    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
    commission_id TEXT NOT NULL REFERENCES commissions(id),
    event_type TEXT NOT NULL CHECK (event_type IN (
        'commission_proposed', 'commission_accepted', 'commission_amended', 'assignment_ready',
        'attempt_started', 'result_submitted', 'result_accepted', 'result_integrated',
        'evidence_recorded',
        'commission_verified_complete', 'assignment_blocked',
        'attachment_joined', 'active_attachment_changed'
    )),
    commission_revision INTEGER NOT NULL,
    payload_json TEXT NOT NULL DEFAULT '{}',
    created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS idempotency_keys (
    key TEXT PRIMARY KEY,
    request_hash TEXT NOT NULL,
    response_json TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
"#;

pub(super) fn migrate(connection: &Connection) -> Result<(), TyrionError> {
    if !column_exists(connection, "commissions", "control_revision")? {
        connection.execute(
            "ALTER TABLE commissions ADD COLUMN control_revision INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    if !column_exists(connection, "commissions", "execution_json")? {
        connection.execute(
            "ALTER TABLE commissions ADD COLUMN execution_json TEXT NOT NULL DEFAULT '{\"kind\":\"deterministic\"}'",
            [],
        )?;
    }
    upgrade_criteria(connection)?;
    backfill_criterion_versions(connection)?;
    upgrade_assignments(connection)?;
    upgrade_attempts(connection)?;
    upgrade_evidence(connection)?;
    connection.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS worker_leases (
            id TEXT PRIMARY KEY,
            attempt_id TEXT NOT NULL UNIQUE REFERENCES attempts(id),
            issued_at INTEGER NOT NULL,
            expires_at INTEGER NOT NULL,
            released_at INTEGER,
            status TEXT NOT NULL CHECK (status IN ('active', 'released', 'revoked', 'expired'))
        );
        "#,
    )?;
    add_result_columns(connection)?;
    upgrade_results(connection)?;
    let events_schema = connection
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'events'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    let needs_event_upgrade = events_schema.is_some_and(|schema| {
        !schema.contains("active_attachment_changed")
            || !schema.contains("payload_json")
            || !schema.contains("result_integrated")
            || !schema.contains("commission_amended")
    });
    if needs_event_upgrade {
        connection.execute_batch(
            r#"
            BEGIN IMMEDIATE;
            ALTER TABLE events RENAME TO events_before_attachments;
            CREATE TABLE events (
                sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                commission_id TEXT NOT NULL REFERENCES commissions(id),
                event_type TEXT NOT NULL CHECK (event_type IN (
                    'commission_proposed', 'commission_accepted', 'commission_amended', 'assignment_ready',
                    'attempt_started', 'result_submitted', 'result_accepted', 'result_integrated',
                    'evidence_recorded',
                    'commission_verified_complete', 'assignment_blocked',
                    'attachment_joined', 'active_attachment_changed'
                )),
                commission_revision INTEGER NOT NULL,
                payload_json TEXT NOT NULL DEFAULT '{}',
                created_at INTEGER NOT NULL
            );
            INSERT INTO events (
                sequence, commission_id, event_type, commission_revision, payload_json, created_at
            )
            SELECT sequence, commission_id, event_type, commission_revision, '{}', created_at
            FROM events_before_attachments;
            DROP TABLE events_before_attachments;
            COMMIT;
            "#,
        )?;
    }
    connection.pragma_update(None, "user_version", 5)?;
    Ok(())
}

pub(super) fn migration_required(connection: &Connection) -> Result<bool, TyrionError> {
    if !table_exists(connection, "commissions")? {
        return Ok(false);
    }
    let user_version =
        connection.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))?;
    let events_schema = table_schema(connection, "events")?;
    let criteria_schema = table_schema(connection, "criteria")?;
    let assignments_schema = table_schema(connection, "assignments")?;
    let attempts_schema = table_schema(connection, "attempts")?;
    let evidence_schema = table_schema(connection, "evidence")?;
    let results_schema = table_schema(connection, "results")?;
    Ok(user_version < 5
        || !column_exists(connection, "commissions", "control_revision")?
        || !column_exists(connection, "commissions", "execution_json")?
        || !column_exists(connection, "attachments", "session_token_hash")?
        || !column_exists(connection, "results", "integrated_artifact_revision")?
        || results_schema.is_some_and(|schema| !schema.contains("'superseded'"))
        || !table_exists(connection, "worker_leases")?
        || !table_exists(connection, "criterion_versions")?
        || !table_exists(connection, "verification_gates")?
        || !table_exists(connection, "verification_recoveries")?
        || criteria_schema.is_some_and(|schema| !schema.contains("required_evidence"))
        || assignments_schema.is_some_and(|schema| !schema.contains("'verification_pending'"))
        || attempts_schema.is_some_and(|schema| !schema.contains("'failed'"))
        || evidence_schema.is_some_and(|schema| !schema.contains("verification_attempt_id"))
        || events_schema.is_some_and(|schema| {
            !schema.contains("active_attachment_changed")
                || !schema.contains("payload_json")
                || !schema.contains("result_integrated")
                || !schema.contains("commission_amended")
        }))
}

pub(super) fn migration_backup_path(database_path: &Path) -> Result<PathBuf, TyrionError> {
    let file_name = database_path
        .file_name()
        .ok_or_else(|| TyrionError::InvalidRequest("database path must have a file name".into()))?;
    let backup_name = format!("{}.pre-migration-v5", file_name.to_string_lossy());
    Ok(database_path.with_file_name(backup_name))
}

pub(super) fn create_backup(
    connection: &Connection,
    backup_path: &Path,
) -> Result<(), TyrionError> {
    if backup_path.exists() {
        return Err(TyrionError::InvalidRequest(format!(
            "refusing to overwrite pre-migration backup {}",
            backup_path.display()
        )));
    }
    let mut destination = Connection::open(backup_path)?;
    {
        let backup = Backup::new(connection, &mut destination)?;
        backup.run_to_completion(100, Duration::from_millis(10), None)?;
    }
    verify_integrity(&destination)?;
    Ok(())
}

pub(super) fn verify(connection: &Connection) -> Result<(), TyrionError> {
    verify_integrity(connection)?;
    let user_version =
        connection.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))?;
    if user_version != 5
        || !column_exists(connection, "commissions", "control_revision")?
        || !column_exists(connection, "commissions", "execution_json")?
        || !column_exists(connection, "attachments", "session_token_hash")?
        || !column_exists(connection, "results", "integrated_artifact_revision")?
        || !table_exists(connection, "worker_leases")?
        || !table_exists(connection, "criterion_versions")?
        || !table_exists(connection, "verification_gates")?
        || !table_exists(connection, "verification_recoveries")?
    {
        return Err(TyrionError::InvalidRequest(
            "schema migration verification failed".into(),
        ));
    }
    let events_schema = table_schema(connection, "events")?.ok_or_else(|| {
        TyrionError::InvalidRequest("schema migration did not create events".into())
    })?;
    let criteria_schema = table_schema(connection, "criteria")?.ok_or_else(|| {
        TyrionError::InvalidRequest("schema migration did not create criteria".into())
    })?;
    let assignments_schema = table_schema(connection, "assignments")?.ok_or_else(|| {
        TyrionError::InvalidRequest("schema migration did not create assignments".into())
    })?;
    let attempts_schema = table_schema(connection, "attempts")?.ok_or_else(|| {
        TyrionError::InvalidRequest("schema migration did not create attempts".into())
    })?;
    let evidence_schema = table_schema(connection, "evidence")?.ok_or_else(|| {
        TyrionError::InvalidRequest("schema migration did not create evidence".into())
    })?;
    let results_schema = table_schema(connection, "results")?.ok_or_else(|| {
        TyrionError::InvalidRequest("schema migration did not create results".into())
    })?;
    if !events_schema.contains("active_attachment_changed")
        || !events_schema.contains("payload_json")
        || !events_schema.contains("result_integrated")
        || !events_schema.contains("commission_amended")
    {
        return Err(TyrionError::InvalidRequest(
            "events schema migration verification failed".into(),
        ));
    }
    if !criteria_schema.contains("required_evidence")
        || !criteria_schema.contains("'prompt'")
        || !assignments_schema.contains("'verification_pending'")
        || !attempts_schema.contains("'failed'")
        || !results_schema.contains("'superseded'")
        || !evidence_schema.contains("verification_attempt_id")
    {
        return Err(TyrionError::InvalidRequest(
            "lifecycle schema migration verification failed".into(),
        ));
    }
    Ok(())
}

fn upgrade_criteria(connection: &Connection) -> Result<(), TyrionError> {
    let Some(schema) = table_schema(connection, "criteria")? else {
        return Ok(());
    };
    if schema.contains("required_evidence") {
        return Ok(());
    }
    connection.execute_batch(
        r#"
        PRAGMA foreign_keys = OFF;
        BEGIN IMMEDIATE;
        CREATE TABLE criteria_v5 (
            commission_id TEXT NOT NULL REFERENCES commissions(id),
            criterion_id TEXT NOT NULL,
            position INTEGER NOT NULL,
            description TEXT NOT NULL,
            required_evidence TEXT NOT NULL,
            verifier_type TEXT NOT NULL CHECK (verifier_type IN ('deterministic', 'model', 'principal')),
            verification_depth TEXT NOT NULL CHECK (verification_depth IN ('standard', 'independent')),
            verifier_configuration TEXT NOT NULL,
            verification_environment TEXT NOT NULL,
            verifier_kind TEXT NOT NULL CHECK (verifier_kind IN ('exact_match', 'command', 'prompt')),
            expected TEXT NOT NULL,
            status TEXT NOT NULL CHECK (status IN ('uncertain', 'passed', 'failed')),
            PRIMARY KEY (commission_id, criterion_id)
        );
        INSERT INTO criteria_v5 (
            commission_id, criterion_id, position, description, required_evidence,
            verifier_type, verification_depth, verifier_configuration,
            verification_environment, verifier_kind, expected, status
        )
        SELECT commission_id, criterion_id, position, description, 'verifier_output',
               'deterministic', 'standard',
               CASE verifier_kind
                   WHEN 'exact_match' THEN 'deterministic-exact-match-v1'
                   ELSE 'contained-command-v1'
               END,
               'tyrion-controlled-v1', verifier_kind, expected,
               CASE status WHEN 'pending' THEN 'uncertain' ELSE status END
        FROM criteria;
        DROP TABLE criteria;
        ALTER TABLE criteria_v5 RENAME TO criteria;
        COMMIT;
        PRAGMA foreign_keys = ON;
        "#,
    )?;
    Ok(())
}

fn backfill_criterion_versions(connection: &Connection) -> Result<(), TyrionError> {
    connection.execute(
        "INSERT INTO criterion_versions (
            commission_id, mandate_revision, criterion_id, position, description,
            required_evidence, verifier_type, verification_depth,
            verifier_configuration, verification_environment, verifier_kind, expected
         )
         SELECT criteria.commission_id,
                CASE commissions.status
                    WHEN 'proposed' THEN 0
                    WHEN 'verified_complete' THEN commissions.revision - 1
                    ELSE commissions.revision
                END,
                criteria.criterion_id, criteria.position, criteria.description,
                criteria.required_evidence, criteria.verifier_type,
                criteria.verification_depth, criteria.verifier_configuration,
                criteria.verification_environment, criteria.verifier_kind, criteria.expected
         FROM criteria
         JOIN commissions ON commissions.id = criteria.commission_id
         WHERE NOT EXISTS (
             SELECT 1 FROM criterion_versions
             WHERE criterion_versions.commission_id = criteria.commission_id
         )",
        [],
    )?;
    Ok(())
}

fn upgrade_assignments(connection: &Connection) -> Result<(), TyrionError> {
    let Some(schema) = table_schema(connection, "assignments")? else {
        return Ok(());
    };
    if schema.contains("'verification_pending'") {
        return Ok(());
    }
    connection.execute_batch(
        r#"
        PRAGMA foreign_keys = OFF;
        BEGIN IMMEDIATE;
        CREATE TABLE assignments_v5 (
            id TEXT PRIMARY KEY,
            commission_id TEXT NOT NULL REFERENCES commissions(id),
            plan_revision INTEGER NOT NULL,
            status TEXT NOT NULL CHECK (status IN (
                'ready', 'running', 'accepted', 'verification_pending',
                'verification_failed', 'resource_blocked'
            )),
            created_at INTEGER NOT NULL
        );
        INSERT INTO assignments_v5 SELECT * FROM assignments;
        DROP TABLE assignments;
        ALTER TABLE assignments_v5 RENAME TO assignments;
        COMMIT;
        PRAGMA foreign_keys = ON;
        "#,
    )?;
    Ok(())
}

fn upgrade_attempts(connection: &Connection) -> Result<(), TyrionError> {
    let Some(schema) = table_schema(connection, "attempts")? else {
        return Ok(());
    };
    if schema.contains("'failed'") {
        return Ok(());
    }
    connection.execute_batch(
        r#"
        PRAGMA foreign_keys = OFF;
        BEGIN IMMEDIATE;
        CREATE TABLE attempts_v5 (
            id TEXT PRIMARY KEY,
            assignment_id TEXT NOT NULL REFERENCES assignments(id),
            worker_configuration TEXT NOT NULL,
            status TEXT NOT NULL CHECK (status IN ('running', 'succeeded', 'failed')),
            started_at INTEGER NOT NULL,
            completed_at INTEGER
        );
        INSERT INTO attempts_v5 SELECT * FROM attempts;
        DROP TABLE attempts;
        ALTER TABLE attempts_v5 RENAME TO attempts;
        COMMIT;
        PRAGMA foreign_keys = ON;
        "#,
    )?;
    Ok(())
}

fn upgrade_evidence(connection: &Connection) -> Result<(), TyrionError> {
    let Some(schema) = table_schema(connection, "evidence")? else {
        return Ok(());
    };
    if schema.contains("verification_attempt_id") {
        return Ok(());
    }
    connection.execute_batch(
        r#"
        PRAGMA foreign_keys = OFF;
        BEGIN IMMEDIATE;
        CREATE TABLE evidence_v5 (
            id TEXT PRIMARY KEY,
            commission_id TEXT NOT NULL REFERENCES commissions(id),
            criterion_id TEXT NOT NULL,
            result_id TEXT NOT NULL REFERENCES results(id),
            mandate_revision INTEGER NOT NULL,
            artifact_revision TEXT NOT NULL,
            evidence_type TEXT NOT NULL,
            verifier_type TEXT NOT NULL CHECK (verifier_type IN ('deterministic', 'model', 'principal')),
            scope TEXT NOT NULL CHECK (scope IN ('candidate', 'integrated', 'external')),
            verification_attempt_id TEXT NOT NULL,
            verifier_identity TEXT NOT NULL,
            verifier_configuration TEXT NOT NULL,
            verifier_kind TEXT NOT NULL CHECK (verifier_kind IN ('exact_match', 'command', 'prompt')),
            procedure_json TEXT NOT NULL,
            environment TEXT NOT NULL,
            outcome TEXT NOT NULL CHECK (outcome IN ('passed', 'failed', 'uncertain')),
            observed TEXT NOT NULL,
            expected TEXT NOT NULL,
            material_contradiction INTEGER NOT NULL CHECK (material_contradiction IN (0, 1)),
            defect TEXT CHECK (defect IN ('result', 'verifier', 'environment', 'criterion')),
            producer_attempt_id TEXT,
            created_at INTEGER NOT NULL,
            UNIQUE (commission_id, criterion_id, evidence_type, verification_attempt_id),
            FOREIGN KEY (commission_id, criterion_id) REFERENCES criteria(commission_id, criterion_id)
        );
        INSERT INTO evidence_v5 (
            id, commission_id, criterion_id, result_id, mandate_revision,
            artifact_revision, evidence_type, verifier_type, scope,
            verification_attempt_id, verifier_identity, verifier_configuration,
            verifier_kind, procedure_json, environment, outcome, observed,
            expected, material_contradiction, defect, producer_attempt_id, created_at
        )
        SELECT id, commission_id, criterion_id, result_id, mandate_revision,
               artifact_revision, 'verifier_output', 'deterministic', 'integrated',
               'legacy-' || id, verifier_kind, 'legacy-v4', verifier_kind,
               json_quote(expected), 'tyrion-controlled-v1', outcome, observed,
               expected, 0, CASE outcome WHEN 'failed' THEN 'result' ELSE NULL END,
               NULL, created_at
        FROM evidence;
        DROP TABLE evidence;
        ALTER TABLE evidence_v5 RENAME TO evidence;
        CREATE TRIGGER evidence_is_immutable_update
        BEFORE UPDATE ON evidence
        BEGIN
            SELECT RAISE(ABORT, 'Evidence is immutable');
        END;
        CREATE TRIGGER evidence_is_immutable_delete
        BEFORE DELETE ON evidence
        BEGIN
            SELECT RAISE(ABORT, 'Evidence is immutable');
        END;
        COMMIT;
        PRAGMA foreign_keys = ON;
        "#,
    )?;
    Ok(())
}

fn add_result_columns(connection: &Connection) -> Result<(), TyrionError> {
    let columns = [
        ("mandate_revision", "INTEGER"),
        ("base_revision", "TEXT"),
        ("candidate_commits_json", "TEXT NOT NULL DEFAULT '[]'"),
        ("changed_paths_json", "TEXT NOT NULL DEFAULT '[]'"),
        ("artifacts_json", "TEXT NOT NULL DEFAULT '[]'"),
        ("verification_outcomes_json", "TEXT NOT NULL DEFAULT '[]'"),
        ("known_effects_json", "TEXT NOT NULL DEFAULT '[]'"),
        ("integrated_artifact_revision", "TEXT"),
    ];
    for (name, definition) in columns {
        if !column_exists(connection, "results", name)? {
            connection.execute(
                &format!("ALTER TABLE results ADD COLUMN {name} {definition}"),
                [],
            )?;
        }
    }
    Ok(())
}

fn upgrade_results(connection: &Connection) -> Result<(), TyrionError> {
    let Some(schema) = table_schema(connection, "results")? else {
        return Ok(());
    };
    if schema.contains("'superseded'") {
        return Ok(());
    }
    connection.execute_batch(
        r#"
        PRAGMA foreign_keys = OFF;
        BEGIN IMMEDIATE;
        CREATE TABLE results_v5 (
            id TEXT PRIMARY KEY,
            attempt_id TEXT NOT NULL REFERENCES attempts(id),
            output TEXT NOT NULL,
            artifact_revision TEXT NOT NULL,
            status TEXT NOT NULL CHECK (status IN ('candidate', 'accepted', 'superseded')),
            created_at INTEGER NOT NULL,
            mandate_revision INTEGER,
            base_revision TEXT,
            candidate_commits_json TEXT NOT NULL DEFAULT '[]',
            changed_paths_json TEXT NOT NULL DEFAULT '[]',
            artifacts_json TEXT NOT NULL DEFAULT '[]',
            verification_outcomes_json TEXT NOT NULL DEFAULT '[]',
            known_effects_json TEXT NOT NULL DEFAULT '[]',
            integrated_artifact_revision TEXT
        );
        INSERT INTO results_v5 SELECT * FROM results;
        DROP TABLE results;
        ALTER TABLE results_v5 RENAME TO results;
        COMMIT;
        PRAGMA foreign_keys = ON;
        "#,
    )?;
    Ok(())
}

fn verify_integrity(connection: &Connection) -> Result<(), TyrionError> {
    let result =
        connection.query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))?;
    if result != "ok" {
        return Err(TyrionError::InvalidRequest(format!(
            "database integrity check failed: {result}"
        )));
    }
    let foreign_key_violation = connection
        .query_row(
            "SELECT \"table\", rowid, parent, fkid FROM pragma_foreign_key_check LIMIT 1",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            },
        )
        .optional()?;
    if let Some((table, rowid, parent, fkid)) = foreign_key_violation {
        return Err(TyrionError::InvalidRequest(format!(
            "foreign-key integrity failed for {table} row {rowid}, parent {parent}, key {fkid}"
        )));
    }
    Ok(())
}

fn table_exists(connection: &Connection, table: &str) -> Result<bool, TyrionError> {
    Ok(table_schema(connection, table)?.is_some())
}

fn table_schema(connection: &Connection, table: &str) -> Result<Option<String>, TyrionError> {
    Ok(connection
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [table],
            |row| row.get::<_, String>(0),
        )
        .optional()?)
}

fn column_exists(connection: &Connection, table: &str, column: &str) -> Result<bool, TyrionError> {
    let exists = connection.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM pragma_table_info(?1) WHERE name = ?2
         )",
        [table, column],
        |row| row.get::<_, bool>(0),
    )?;
    Ok(exists)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    #[test]
    fn store_open_uses_verified_temporary_backup() {
        let temp = TempDir::new().expect("temporary directory should be created");
        let database_path = temp.path().join("state.sqlite3");
        let connection = Connection::open(&database_path).expect("legacy database should open");
        connection
            .execute_batch(
                r#"
                CREATE TABLE commissions (
                    id TEXT PRIMARY KEY,
                    goal TEXT NOT NULL,
                    status TEXT NOT NULL,
                    revision INTEGER NOT NULL,
                    created_at INTEGER NOT NULL,
                    accepted_at INTEGER,
                    completed_at INTEGER,
                    artifact_revision TEXT
                );
                CREATE TABLE events (
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
                PRAGMA user_version = 1;
                "#,
            )
            .expect("legacy schema should be created");
        assert!(migration_required(&connection).unwrap());
        drop(connection);

        let backup_path = migration_backup_path(&database_path).unwrap();
        fs::write(&backup_path, b"existing backup").unwrap();
        let error = super::super::Store::open(&database_path)
            .err()
            .expect("startup must refuse to overwrite a migration backup");
        assert!(error.to_string().contains("refusing to overwrite"));
        fs::remove_file(&backup_path).unwrap();

        drop(super::super::Store::open(&database_path).unwrap());
        assert!(!backup_path.exists());
        let connection = Connection::open(database_path).expect("migrated database should open");
        verify(&connection).unwrap();
        assert!(!migration_required(&connection).unwrap());
    }
}
