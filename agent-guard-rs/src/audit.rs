use crate::core::{CanonicalEvent, Decision, Finding};
use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};

pub struct AuditStore {
    connection: Connection,
    path: PathBuf,
}

impl AuditStore {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let connection = Connection::open(path)
            .with_context(|| format!("failed to open audit database {}", path.display()))?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "busy_timeout", 5000)?;
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS events (
                event_id TEXT PRIMARY KEY,
                timestamp TEXT NOT NULL,
                agent_kind TEXT NOT NULL,
                session_id TEXT NOT NULL,
                event_kind TEXT NOT NULL,
                tool_name TEXT,
                payload_json TEXT NOT NULL,
                payload_hash TEXT NOT NULL,
                action TEXT NOT NULL,
                evaluation_ms INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS findings (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                event_id TEXT NOT NULL REFERENCES events(event_id),
                rule_id TEXT NOT NULL,
                title TEXT NOT NULL,
                severity TEXT NOT NULL,
                action TEXT NOT NULL,
                message TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_events_session ON events(session_id, timestamp);
            CREATE INDEX IF NOT EXISTS idx_findings_rule ON findings(rule_id);",
        )?;
        Ok(Self {
            connection,
            path: path.to_path_buf(),
        })
    }

    pub fn record(&mut self, event: &CanonicalEvent, decision: &Decision) -> Result<()> {
        let mut value = serde_json::to_value(event)?;
        redact_value(&mut value);
        let payload = serde_json::to_string(&value)?;
        let payload_hash = format!("{:x}", Sha256::digest(payload.as_bytes()));
        let transaction = self.connection.transaction()?;
        transaction.execute(
            "INSERT INTO events (
                event_id, timestamp, agent_kind, session_id, event_kind, tool_name,
                payload_json, payload_hash, action, evaluation_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                event.event_id.to_string(),
                event.timestamp.to_rfc3339(),
                event.agent.kind,
                event.session.id,
                format!("{:?}", event.event.kind).to_lowercase(),
                event.tool.as_ref().map(|tool| tool.name.as_str()),
                payload,
                payload_hash,
                format!("{:?}", decision.action).to_lowercase(),
                decision.evaluation_ms as i64,
            ],
        )?;
        for finding in &decision.findings {
            insert_finding(&transaction, &event.event_id.to_string(), finding)?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn recent_findings(&self, limit: usize) -> Result<Vec<Value>> {
        let mut statement = self.connection.prepare(
            "SELECT f.event_id, e.timestamp, e.agent_kind, e.tool_name, f.rule_id,
                    f.severity, f.action, f.message
             FROM findings f JOIN events e ON e.event_id = f.event_id
             ORDER BY e.timestamp DESC LIMIT ?1",
        )?;
        let rows = statement.query_map([limit as i64], |row| {
            Ok(serde_json::json!({
                "event_id": row.get::<_, String>(0)?,
                "timestamp": row.get::<_, String>(1)?,
                "agent": row.get::<_, String>(2)?,
                "tool": row.get::<_, Option<String>>(3)?,
                "rule_id": row.get::<_, String>(4)?,
                "severity": row.get::<_, String>(5)?,
                "action": row.get::<_, String>(6)?,
                "message": row.get::<_, String>(7)?,
            }))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn event_count(&self) -> Result<u64> {
        Ok(self
            .connection
            .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))?)
    }

    pub fn export_events(&self) -> Result<Vec<Value>> {
        let mut statement = self.connection.prepare(
            "SELECT payload_json, action, evaluation_ms FROM events ORDER BY timestamp ASC",
        )?;
        let rows = statement.query_map([], |row| {
            let payload: String = row.get(0)?;
            let mut value: Value = serde_json::from_str(&payload).map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    payload.len(),
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            })?;
            if let Some(object) = value.as_object_mut() {
                object.insert("decision_action".to_owned(), Value::String(row.get(1)?));
                object.insert(
                    "evaluation_ms".to_owned(),
                    Value::Number(serde_json::Number::from(row.get::<_, i64>(2)?)),
                );
            }
            Ok(value)
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn insert_finding(connection: &Connection, event_id: &str, finding: &Finding) -> Result<()> {
    connection.execute(
        "INSERT INTO findings (event_id, rule_id, title, severity, action, message)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            event_id,
            finding.rule_id,
            finding.title,
            format!("{:?}", finding.severity).to_lowercase(),
            format!("{:?}", finding.action).to_lowercase(),
            finding.message,
        ],
    )?;
    Ok(())
}

fn redact_value(value: &mut Value) {
    match value {
        Value::Object(object) => {
            for (key, value) in object.iter_mut() {
                let normalized = key.to_ascii_lowercase();
                if [
                    "password",
                    "passwd",
                    "secret",
                    "token",
                    "authorization",
                    "api_key",
                ]
                .iter()
                .any(|sensitive| normalized.contains(sensitive))
                {
                    *value = Value::String("[REDACTED]".to_owned());
                } else {
                    redact_value(value);
                }
            }
        }
        Value::Array(values) => values.iter_mut().for_each(redact_value),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude::normalize_event;
    use crate::core::{Action, Decision};
    use serde_json::json;
    use tempfile::TempDir;

    #[test]
    fn records_event_and_redacts_sensitive_fields() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("audit.db");
        let mut store = AuditStore::open(&path).unwrap();
        let event = normalize_event(json!({
            "hook_event_name": "PreToolUse",
            "session_id": "s1",
            "tool_name": "Bash",
            "tool_input": {"api_token": "do-not-store"}
        }))
        .unwrap();
        store
            .record(
                &event,
                &Decision {
                    action: Action::Allow,
                    findings: vec![],
                    evaluation_ms: 1,
                },
            )
            .unwrap();
        assert_eq!(store.event_count().unwrap(), 1);
        let payload: String = store
            .connection
            .query_row("SELECT payload_json FROM events", [], |row| row.get(0))
            .unwrap();
        assert!(!payload.contains("do-not-store"));
        assert!(payload.contains("[REDACTED]"));
    }
}
