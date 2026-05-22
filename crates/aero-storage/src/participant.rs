//! Participant + credential repositories. Implementation is delegated to the
//! storage agent (see Spec §3.3).

use aero_common::{Participant, ParticipantId, ParticipantKind};
use sqlx::PgPool;

#[derive(Clone)]
pub struct ParticipantRepo {
    pool: PgPool,
}

#[derive(Debug, Clone)]
pub struct NewHuman {
    pub email: String,
    pub display_name: String,
    pub password_hash: String,
}

#[derive(Debug, Clone)]
pub struct CredentialRecord {
    pub participant_id: ParticipantId,
    pub email: String,
    pub password_hash: String,
}

impl ParticipantRepo {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Inserts a new Human participant + credentials row in one transaction.
    /// Returns the created `Participant`. Email is unique.
    pub async fn create_human(&self, new: NewHuman) -> Result<Participant, sqlx::Error> {
        let id = ParticipantId::new();
        let mut tx = self.pool.begin().await?;
        let created_at = time::OffsetDateTime::now_utc();

        sqlx::query(
            r#"INSERT INTO participants (id, kind, display_name, avatar_url, created_by, created_at)
               VALUES ($1, 'human', $2, NULL, NULL, $3)"#,
        )
        .bind(id.to_uuid())
        .bind(&new.display_name)
        .bind(created_at)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            r#"INSERT INTO credentials (participant_id, email, password_hash, created_at)
               VALUES ($1, $2, $3, $4)"#,
        )
        .bind(id.to_uuid())
        .bind(&new.email)
        .bind(&new.password_hash)
        .bind(created_at)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(Participant {
            id,
            kind: ParticipantKind::Human,
            display_name: new.display_name,
            avatar_url: None,
            created_by: None,
            created_at,
        })
    }

    pub async fn find_credentials_by_email(
        &self,
        email: &str,
    ) -> Result<Option<CredentialRecord>, sqlx::Error> {
        let row = sqlx::query_as::<_, (uuid::Uuid, String, String)>(
            r#"SELECT participant_id, email, password_hash FROM credentials WHERE email = $1"#,
        )
        .bind(email)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|(pid, email, hash)| CredentialRecord {
            participant_id: ParticipantId::from_uuid(pid),
            email,
            password_hash: hash,
        }))
    }

    pub async fn get(&self, id: ParticipantId) -> Result<Option<Participant>, sqlx::Error> {
        let row = sqlx::query_as::<_, (uuid::Uuid, String, String, Option<String>, Option<uuid::Uuid>, time::OffsetDateTime)>(
            r#"SELECT id, kind, display_name, avatar_url, created_by, created_at
               FROM participants WHERE id = $1"#,
        )
        .bind(id.to_uuid())
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|(id, kind, name, avatar, creator, created_at)| {
            let kind = match kind.as_str() {
                "human" => ParticipantKind::Human,
                "agent" => ParticipantKind::Agent,
                _ => ParticipantKind::Bot,
            };
            Participant {
                id: ParticipantId::from_uuid(id),
                kind,
                display_name: name,
                avatar_url: avatar,
                created_by: creator.map(ParticipantId::from_uuid),
                created_at,
            }
        }))
    }
}
