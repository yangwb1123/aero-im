//! Message-template / canned-response repository (per-user reusable bodies).
//!
//! Backs `migrations/0046_message_templates.sql`. A user saves a reusable
//! message body (a JSON array of blocks) under a name, then lists, deletes, or
//! posts it into a room with one call. This repo owns only the template CRUD —
//! *sending* a template replays its stored blocks through the normal send path
//! ([`ImService::send_message`](aero_im_core::ImService::send_message)), so the
//! usual room-membership / post-policy / moderation gates still apply.
//!
//! Every read/mutate method is owner-scoped (`participant_id` in the `WHERE`), so
//! a caller can only ever see, delete, or send their own templates. Purely
//! additive: a NEW [`MessageTemplateRepo`]; no existing repo is touched. The
//! [`MessageTemplate`] model lives here (and is re-exported from the crate root)
//! rather than in `aero-common`, since it is a storage-layer projection.

use aero_common::{MessageTemplateId, ParticipantId};
use serde::Serialize;
use sqlx::PgPool;

/// One message template — a per-user, reusable message body.
///
/// A storage-layer projection of a `message_templates` row. `Serialize` so a
/// handler can hand the row straight back as JSON; `created_at` renders as
/// RFC 3339, and `blocks` is the raw stored payload (a JSON array of blocks).
#[derive(Debug, Clone, Serialize)]
pub struct MessageTemplate {
    /// The template's unique id.
    pub id: MessageTemplateId,
    /// The owner the template belongs to (and is scoped to).
    pub participant_id: ParticipantId,
    /// Human-readable name the owner gave the template.
    pub name: String,
    /// The message payload (a JSON array of blocks), replayed on send.
    pub blocks: serde_json::Value,
    /// When the template was created (RFC 3339 on the wire).
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: time::OffsetDateTime,
}

/// The columns a [`MessageTemplate`] is built from, in select order. Shared by
/// every query so the row decoding stays in one place.
const COLUMNS: &str = "id, participant_id, name, blocks, created_at";

#[derive(Debug, Clone, sqlx::FromRow)]
struct Row {
    id: uuid::Uuid,
    participant_id: uuid::Uuid,
    name: String,
    blocks: serde_json::Value,
    created_at: time::OffsetDateTime,
}

fn row_to_model(r: Row) -> MessageTemplate {
    MessageTemplate {
        id: MessageTemplateId::from_uuid(r.id),
        participant_id: ParticipantId::from_uuid(r.participant_id),
        name: r.name,
        blocks: r.blocks,
        created_at: r.created_at,
    }
}

/// Repository over the `message_templates` table (per-user canned responses).
///
/// Cheap to clone — it just wraps a [`PgPool`] (itself an `Arc` internally), so
/// feature modules build one inline via [`MessageTemplateRepo::new`].
#[derive(Clone)]
#[must_use]
pub struct MessageTemplateRepo {
    pool: PgPool,
}

impl MessageTemplateRepo {
    /// Build a repo over the given pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Persist a new template for `participant`, returning its generated id. The
    /// caller is responsible for name/blocks validation.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the insert.
    pub async fn create(
        &self,
        participant: ParticipantId,
        name: &str,
        blocks: &serde_json::Value,
    ) -> Result<MessageTemplateId, sqlx::Error> {
        let id = MessageTemplateId::new();
        sqlx::query(
            r"INSERT INTO message_templates (id, participant_id, name, blocks)
               VALUES ($1, $2, $3, $4)",
        )
        .bind(id.to_uuid())
        .bind(participant.to_uuid())
        .bind(name)
        .bind(blocks)
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    /// List `participant`'s templates, newest first. Owner-scoped — only the
    /// caller's own rows are returned.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn list_for(
        &self,
        participant: ParticipantId,
    ) -> Result<Vec<MessageTemplate>, sqlx::Error> {
        let sql = format!(
            "SELECT {COLUMNS}
               FROM message_templates
              WHERE participant_id = $1
              ORDER BY created_at DESC, id DESC"
        );
        let rows = sqlx::query_as::<_, Row>(&sql)
            .bind(participant.to_uuid())
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(row_to_model).collect())
    }

    /// Fetch one of `participant`'s templates by id, or `None` if no such row
    /// exists *for that owner*. Owner-scoped: a stranger's id resolves to `None`,
    /// so this can never surface another user's template.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn get(
        &self,
        id: MessageTemplateId,
        participant: ParticipantId,
    ) -> Result<Option<MessageTemplate>, sqlx::Error> {
        let sql = format!(
            "SELECT {COLUMNS} FROM message_templates WHERE id = $1 AND participant_id = $2"
        );
        let row = sqlx::query_as::<_, Row>(&sql)
            .bind(id.to_uuid())
            .bind(participant.to_uuid())
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(row_to_model))
    }

    /// Delete one of `participant`'s templates. Returns `true` iff a row was
    /// removed — owner-scoped, so a caller can never delete another user's
    /// template, and a second delete (or a stranger's) is a no-op returning
    /// `false`.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the delete.
    pub async fn delete(
        &self,
        id: MessageTemplateId,
        participant: ParticipantId,
    ) -> Result<bool, sqlx::Error> {
        let result =
            sqlx::query("DELETE FROM message_templates WHERE id = $1 AND participant_id = $2")
                .bind(id.to_uuid())
                .bind(participant.to_uuid())
                .execute(&self.pool)
                .await?;
        Ok(result.rows_affected() > 0)
    }
}

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored message_template
/// ```
#[cfg(test)]
mod db_tests {
    use super::*;

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL")
    }

    /// Create a throwaway owner participant so the test is self-contained.
    async fn owner(p: &PgPool) -> ParticipantId {
        let id = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
            .bind(id.to_uuid())
            .bind(format!("message-template-owner-{id}"))
            .execute(p)
            .await
            .expect("insert participant");
        id
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn message_template_create_list_get_delete_owner_scoped() {
        let p = pool();
        let repo = MessageTemplateRepo::new(p.clone());
        let owner = owner(&p).await;
        let stranger = ParticipantId::new();
        let blocks = serde_json::json!([{ "type": "text", "content": "on it!" }]);

        // create → list shows it.
        let id = repo.create(owner, "ack", &blocks).await.unwrap();
        let listed = repo.list_for(owner).await.unwrap();
        assert!(listed.iter().any(|t| t.id == id), "list shows the template");
        let found = listed.iter().find(|t| t.id == id).expect("present");
        assert_eq!(found.name, "ack");
        assert_eq!(found.blocks, blocks);

        // get (owner) works; get (stranger) is None.
        let got = repo.get(id, owner).await.unwrap().expect("owner can get");
        assert_eq!(got.id, id);
        assert_eq!(got.blocks, blocks);
        assert!(
            repo.get(id, stranger).await.unwrap().is_none(),
            "stranger cannot get another user's template"
        );

        // A stranger's delete is a no-op; the owner's first delete succeeds, the
        // second is a no-op.
        assert!(
            !repo.delete(id, stranger).await.unwrap(),
            "stranger cannot delete another user's template"
        );
        assert!(repo.delete(id, owner).await.unwrap(), "owner deletes");
        assert!(
            !repo.delete(id, owner).await.unwrap(),
            "second delete is a no-op"
        );
        assert!(
            !repo.list_for(owner).await.unwrap().iter().any(|t| t.id == id),
            "deleted template leaves the list"
        );

        // Cleanup so reruns stay self-contained.
        sqlx::query("DELETE FROM message_templates WHERE participant_id = $1")
            .bind(owner.to_uuid())
            .execute(&p)
            .await
            .ok();
    }
}
