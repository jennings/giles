//! `outbox`: proactive sends (job output, approval prompts, login codes) awaiting delivery to an
//! identity, retried until the channel accepts them or they expire (PLAN 6.11, 8.5).
//!
//! One sender loop in the core drives it: [`Store::outbox_due`] for rows to try now,
//! [`Store::outbox_delete`] on success, [`Store::outbox_retry`] on failure, and
//! [`Store::outbox_expire`] to drop and log expired rows. There are no leases, so only one loop
//! may run.

use giles_types::{NativeId, Timestamp, UserId};
use rusqlite::{Row, params};
use serde_json::Value;

use crate::time::to_millis;
use crate::{Error, Result, Store, get_time, parse_id, query_all, row_id};

row_id!(
    /// A row in `outbox`.
    OutboxId
);

/// A send to enqueue with [`Store::outbox_enqueue`].
#[derive(Debug, Clone, PartialEq)]
pub struct NewOutbox {
    /// The channel instance to send through.
    pub instance: String,
    /// The identity to send to (`SendRequest.recipient`).
    pub recipient: NativeId,
    /// What it is, for logs: e.g. `job`, `approval`, `login_code`.
    pub kind: String,
    /// The user it's for, if any.
    pub user: Option<UserId>,
    /// The message as JSON.
    pub payload: Value,
    /// When it was enqueued; also its first attempt time.
    pub created_at: Timestamp,
    /// When it stops being worth sending: its approval's or code's expiry, else 24 hours after
    /// `created_at`. Must be after `created_at`.
    pub expires_at: Timestamp,
}

/// A row in `outbox`.
#[derive(Debug, Clone, PartialEq)]
pub struct OutboxEntry {
    /// The row.
    pub id: OutboxId,
    /// The channel instance.
    pub instance: String,
    /// The recipient identity.
    pub recipient: NativeId,
    /// What it is.
    pub kind: String,
    /// The user it's for.
    pub user: Option<UserId>,
    /// The message as JSON.
    pub payload: Value,
    /// Failed attempts so far.
    pub attempts: u32,
    /// The error of the last failed attempt.
    pub last_error: Option<String>,
    /// When it was enqueued.
    pub created_at: Timestamp,
    /// When to try next.
    pub next_attempt_at: Timestamp,
    /// When it expires.
    pub expires_at: Timestamp,
}

/// The columns [`from_row`] reads, in order.
macro_rules! columns {
    () => {
        "id, instance, recipient, kind, user_id, payload, attempts, last_error, created_at,
        next_attempt_at, expires_at"
    };
}

const COLUMNS: &str = columns!();
const SELECT: &str = concat!("SELECT ", columns!(), " FROM outbox");

fn from_row(row: &Row<'_>) -> Result<OutboxEntry> {
    Ok(OutboxEntry {
        id: row.get(0)?,
        instance: row.get(1)?,
        recipient: parse_id(row.get(2)?, NativeId::new)?,
        kind: row.get(3)?,
        user: row
            .get::<_, Option<String>>(4)?
            .map(|u| parse_id(u, UserId::new))
            .transpose()?,
        payload: serde_json::from_str(&row.get::<_, String>(5)?)?,
        attempts: u32::try_from(row.get::<_, i64>(6)?)
            .map_err(|_| Error::corrupt("outbox attempts out of range"))?,
        last_error: row.get(7)?,
        created_at: get_time(row, 8)?,
        next_attempt_at: get_time(row, 9)?,
        expires_at: get_time(row, 10)?,
    })
}

impl Store {
    /// Enqueues a send, due immediately (`next_attempt_at = created_at`).
    pub async fn outbox_enqueue(&self, send: NewOutbox) -> Result<OutboxId> {
        if send.expires_at <= send.created_at {
            return Err(Error::invalid(
                "an outbox row must expire after it's created",
            ));
        }
        let payload = serde_json::to_string(&send.payload)?;
        self.write(move |conn| {
            conn.prepare_cached(
                "INSERT INTO outbox (instance, recipient, kind, user_id, payload, created_at,
                    next_attempt_at, expires_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, ?7)",
            )?
            .execute(params![
                send.instance,
                send.recipient.as_str(),
                send.kind,
                send.user.as_ref().map(UserId::as_str),
                payload,
                to_millis(send.created_at),
                to_millis(send.expires_at),
            ])?;
            Ok(OutboxId(conn.last_insert_rowid()))
        })
        .await
    }

    /// Rows to try now: `next_attempt_at <= now < expires_at`, earliest first, at most `limit`.
    pub async fn outbox_due(&self, now: Timestamp, limit: u32) -> Result<Vec<OutboxEntry>> {
        self.read(move |conn| {
            let mut stmt = conn.prepare_cached(&format!(
                "{SELECT} WHERE next_attempt_at <= ?1 AND expires_at > ?1
                 ORDER BY next_attempt_at, id LIMIT ?2"
            ))?;
            query_all(&mut stmt, params![to_millis(now), limit], from_row)
        })
        .await
    }

    /// The next time the outbox needs attention: the earliest `next_attempt_at` or `expires_at`
    /// of any row. `None` when it's empty. The sender sleeps until then (or until something is
    /// enqueued).
    pub async fn outbox_next_wakeup(&self) -> Result<Option<Timestamp>> {
        self.read(|conn| {
            let next: Option<i64> = conn
                .prepare_cached("SELECT min(min(next_attempt_at), min(expires_at)) FROM outbox")?
                .query_row([], |row| row.get(0))?;
            next.map(crate::time::from_millis).transpose()
        })
        .await
    }

    /// Records a failed attempt: increments `attempts`, stores `error`, and schedules the next
    /// try. Returns the new attempt count; [`Error::NotFound`] if the row is gone (delivered,
    /// expired, or deleted meanwhile).
    pub async fn outbox_retry(
        &self,
        id: OutboxId,
        next_attempt_at: Timestamp,
        error: impl Into<String>,
    ) -> Result<u32> {
        let error = error.into();
        self.write(move |conn| {
            let mut stmt = conn.prepare_cached(
                "UPDATE outbox SET attempts = attempts + 1, last_error = ?2, next_attempt_at = ?3
                 WHERE id = ?1 RETURNING attempts",
            )?;
            let mut rows = stmt.query(params![id, error, to_millis(next_attempt_at)])?;
            let row = rows
                .next()?
                .ok_or_else(|| Error::not_found(format!("outbox row {id}")))?;
            u32::try_from(row.get::<_, i64>(0)?)
                .map_err(|_| Error::corrupt("outbox attempts out of range"))
        })
        .await
    }

    /// Deletes a row: it was delivered, or what it announced (an approval) was resolved.
    /// Returns `false` if it was already gone.
    pub async fn outbox_delete(&self, id: OutboxId) -> Result<bool> {
        self.write(move |conn| {
            Ok(conn
                .prepare_cached("DELETE FROM outbox WHERE id = ?1")?
                .execute([id])?
                == 1)
        })
        .await
    }

    /// Deletes every row with `expires_at <= now` and returns them, oldest first, so the core can
    /// log each with its instance, recipient, and kind.
    pub async fn outbox_expire(&self, now: Timestamp) -> Result<Vec<OutboxEntry>> {
        self.write(move |conn| {
            let mut stmt = conn.prepare_cached(&format!(
                "DELETE FROM outbox WHERE expires_at <= ?1 RETURNING {COLUMNS}"
            ))?;
            let mut rows = query_all(&mut stmt, [to_millis(now)], from_row)?;
            rows.sort_by_key(|row| row.id);
            Ok(rows)
        })
        .await
    }

    /// Deletes every row of `kind`, due or not, and returns them, oldest first, for logging like
    /// [`outbox_expire`](Self::outbox_expire). At startup this drops the prompts of approvals
    /// that died with the previous process (pending approvals live in memory, PLAN 6.4).
    pub async fn outbox_expire_kind(&self, kind: impl Into<String>) -> Result<Vec<OutboxEntry>> {
        let kind = kind.into();
        self.write(move |conn| {
            let mut stmt = conn.prepare_cached(&format!(
                "DELETE FROM outbox WHERE kind = ?1 RETURNING {COLUMNS}"
            ))?;
            let mut rows = query_all(&mut stmt, [kind], from_row)?;
            rows.sort_by_key(|row| row.id);
            Ok(rows)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::sessions::tests::{at, store};

    fn send(kind: &str, created: i64, expires: i64) -> NewOutbox {
        NewOutbox {
            instance: "whatsapp".into(),
            recipient: NativeId::new("+15551234567").unwrap(),
            kind: kind.into(),
            user: Some(UserId::new("owner").unwrap()),
            payload: json!({ "markdown": kind }),
            created_at: at(created),
            expires_at: at(expires),
        }
    }

    fn ids(rows: &[OutboxEntry]) -> Vec<OutboxId> {
        rows.iter().map(|row| row.id).collect()
    }

    #[tokio::test]
    async fn rows_are_due_from_creation_until_expiry() {
        let (_dir, store) = store().await;
        assert_eq!(store.outbox_next_wakeup().await.unwrap(), None);
        let a = store.outbox_enqueue(send("job", 10, 100)).await.unwrap();
        let b = store.outbox_enqueue(send("approval", 5, 50)).await.unwrap();

        assert!(store.outbox_due(at(4), 10).await.unwrap().is_empty());
        assert_eq!(ids(&store.outbox_due(at(5), 10).await.unwrap()), [b]);
        let due = store.outbox_due(at(10), 10).await.unwrap();
        assert_eq!(ids(&due), [b, a], "earliest attempt first");
        assert_eq!(ids(&store.outbox_due(at(10), 1).await.unwrap()), [b]);
        assert_eq!(due[1].payload, json!({ "markdown": "job" }));
        assert_eq!((due[1].attempts, due[1].expires_at), (0, at(100)));

        // At its expiry a row is no longer due.
        assert_eq!(ids(&store.outbox_due(at(50), 10).await.unwrap()), [a]);
        assert_eq!(store.outbox_next_wakeup().await.unwrap(), Some(at(5)));
    }

    #[tokio::test]
    async fn retries_back_off_and_count_attempts() {
        let (_dir, store) = store().await;
        let a = store.outbox_enqueue(send("job", 0, 100)).await.unwrap();
        assert_eq!(
            store.outbox_retry(a, at(30), "unavailable").await.unwrap(),
            1
        );
        assert_eq!(
            store
                .outbox_retry(a, at(60), "still unavailable")
                .await
                .unwrap(),
            2
        );
        assert!(store.outbox_due(at(59), 10).await.unwrap().is_empty());
        let due = store.outbox_due(at(60), 10).await.unwrap();
        assert_eq!(due[0].attempts, 2);
        assert_eq!(due[0].last_error.as_deref(), Some("still unavailable"));
        assert_eq!(store.outbox_next_wakeup().await.unwrap(), Some(at(60)));

        assert!(store.outbox_delete(a).await.unwrap());
        assert!(!store.outbox_delete(a).await.unwrap());
        let err = store.outbox_retry(a, at(90), "x").await.unwrap_err();
        assert!(matches!(err, Error::NotFound(_)), "{err}");
    }

    #[tokio::test]
    async fn expiry_removes_and_returns_rows_at_their_deadline() {
        let (_dir, store) = store().await;
        let a = store
            .outbox_enqueue(send("login_code", 0, 10))
            .await
            .unwrap();
        let b = store.outbox_enqueue(send("job", 0, 20)).await.unwrap();
        let c = store.outbox_enqueue(send("job", 0, 30)).await.unwrap();
        // A retry scheduled after expiry: the expiry is the next wakeup.
        store.outbox_retry(c, at(40), "x").await.unwrap();

        assert!(store.outbox_expire(at(9)).await.unwrap().is_empty());
        let expired = store.outbox_expire(at(20)).await.unwrap();
        assert_eq!(ids(&expired), [a, b]);
        assert_eq!(expired[0].kind, "login_code");
        assert_eq!(store.outbox_next_wakeup().await.unwrap(), Some(at(30)));
        assert_eq!(ids(&store.outbox_expire(at(30)).await.unwrap()), [c]);
        assert_eq!(store.outbox_next_wakeup().await.unwrap(), None);
    }

    #[tokio::test]
    async fn expiring_a_kind_drops_its_rows_due_or_not() {
        let (_dir, store) = store().await;
        let a = store
            .outbox_enqueue(send("approval", 0, 100))
            .await
            .unwrap();
        let job = store.outbox_enqueue(send("job", 0, 100)).await.unwrap();
        let b = store
            .outbox_enqueue(send("approval", 0, 100))
            .await
            .unwrap();
        // Not due yet: a retry is scheduled later.
        store.outbox_retry(b, at(90), "x").await.unwrap();

        let expired = store.outbox_expire_kind("approval").await.unwrap();
        assert_eq!(ids(&expired), [a, b]);
        assert_eq!(expired[1].attempts, 1);
        assert_eq!(ids(&store.outbox_due(at(95), 10).await.unwrap()), [job]);
        assert!(
            store
                .outbox_expire_kind("approval")
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn rows_must_expire_after_creation() {
        let (_dir, store) = store().await;
        let err = store.outbox_enqueue(send("job", 10, 10)).await.unwrap_err();
        assert!(matches!(err, Error::InvalidRequest(_)), "{err}");
    }
}
