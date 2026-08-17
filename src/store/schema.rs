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
    verifier_kind TEXT NOT NULL CHECK (verifier_kind IN ('exact_match', 'command')),
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
    status TEXT NOT NULL CHECK (status IN ('candidate', 'accepted')),
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
    verifier_kind TEXT NOT NULL CHECK (verifier_kind IN ('exact_match', 'command')),
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
        'commission_proposed', 'commission_accepted', 'assignment_ready',
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
                    'commission_proposed', 'commission_accepted', 'assignment_ready',
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
    connection.pragma_update(None, "user_version", 4)?;
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
    let attempts_schema = table_schema(connection, "attempts")?;
    let evidence_schema = table_schema(connection, "evidence")?;
    Ok(user_version < 4
        || !column_exists(connection, "commissions", "control_revision")?
        || !column_exists(connection, "commissions", "execution_json")?
        || !column_exists(connection, "attachments", "session_token_hash")?
        || !column_exists(connection, "results", "integrated_artifact_revision")?
        || !table_exists(connection, "worker_leases")?
        || criteria_schema.is_some_and(|schema| !schema.contains("'command'"))
        || attempts_schema.is_some_and(|schema| !schema.contains("'failed'"))
        || evidence_schema.is_some_and(|schema| !schema.contains("'command'"))
        || events_schema.is_some_and(|schema| {
            !schema.contains("active_attachment_changed")
                || !schema.contains("payload_json")
                || !schema.contains("result_integrated")
        }))
}

pub(super) fn migration_backup_path(database_path: &Path) -> Result<PathBuf, TyrionError> {
    let file_name = database_path
        .file_name()
        .ok_or_else(|| TyrionError::InvalidRequest("database path must have a file name".into()))?;
    let backup_name = format!("{}.pre-migration-v4", file_name.to_string_lossy());
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
    if user_version != 4
        || !column_exists(connection, "commissions", "control_revision")?
        || !column_exists(connection, "commissions", "execution_json")?
        || !column_exists(connection, "attachments", "session_token_hash")?
        || !column_exists(connection, "results", "integrated_artifact_revision")?
        || !table_exists(connection, "worker_leases")?
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
    let attempts_schema = table_schema(connection, "attempts")?.ok_or_else(|| {
        TyrionError::InvalidRequest("schema migration did not create attempts".into())
    })?;
    let evidence_schema = table_schema(connection, "evidence")?.ok_or_else(|| {
        TyrionError::InvalidRequest("schema migration did not create evidence".into())
    })?;
    if !events_schema.contains("active_attachment_changed")
        || !events_schema.contains("payload_json")
        || !events_schema.contains("result_integrated")
    {
        return Err(TyrionError::InvalidRequest(
            "events schema migration verification failed".into(),
        ));
    }
    if !criteria_schema.contains("'command'")
        || !attempts_schema.contains("'failed'")
        || !evidence_schema.contains("'command'")
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
    if schema.contains("'command'") {
        return Ok(());
    }
    connection.execute_batch(
        r#"
        PRAGMA foreign_keys = OFF;
        BEGIN IMMEDIATE;
        CREATE TABLE criteria_v4 (
            commission_id TEXT NOT NULL REFERENCES commissions(id),
            criterion_id TEXT NOT NULL,
            position INTEGER NOT NULL,
            description TEXT NOT NULL,
            verifier_kind TEXT NOT NULL CHECK (verifier_kind IN ('exact_match', 'command')),
            expected TEXT NOT NULL,
            status TEXT NOT NULL CHECK (status IN ('pending', 'passed', 'failed')),
            PRIMARY KEY (commission_id, criterion_id)
        );
        INSERT INTO criteria_v4 SELECT * FROM criteria;
        DROP TABLE criteria;
        ALTER TABLE criteria_v4 RENAME TO criteria;
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
        CREATE TABLE attempts_v4 (
            id TEXT PRIMARY KEY,
            assignment_id TEXT NOT NULL REFERENCES assignments(id),
            worker_configuration TEXT NOT NULL,
            status TEXT NOT NULL CHECK (status IN ('running', 'succeeded', 'failed')),
            started_at INTEGER NOT NULL,
            completed_at INTEGER
        );
        INSERT INTO attempts_v4 SELECT * FROM attempts;
        DROP TABLE attempts;
        ALTER TABLE attempts_v4 RENAME TO attempts;
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
    if schema.contains("'command'") {
        return Ok(());
    }
    connection.execute_batch(
        r#"
        PRAGMA foreign_keys = OFF;
        BEGIN IMMEDIATE;
        CREATE TABLE evidence_v4 (
            id TEXT PRIMARY KEY,
            commission_id TEXT NOT NULL REFERENCES commissions(id),
            criterion_id TEXT NOT NULL,
            result_id TEXT NOT NULL REFERENCES results(id),
            mandate_revision INTEGER NOT NULL,
            artifact_revision TEXT NOT NULL,
            verifier_kind TEXT NOT NULL CHECK (verifier_kind IN ('exact_match', 'command')),
            outcome TEXT NOT NULL CHECK (outcome IN ('passed', 'failed')),
            observed TEXT NOT NULL,
            expected TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            FOREIGN KEY (commission_id, criterion_id) REFERENCES criteria(commission_id, criterion_id)
        );
        INSERT INTO evidence_v4 SELECT * FROM evidence;
        DROP TABLE evidence;
        ALTER TABLE evidence_v4 RENAME TO evidence;
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
