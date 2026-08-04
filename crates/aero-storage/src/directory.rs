//! People-directory repository (read-only, workspace-scoped member search).
//!
//! A searchable member directory for a workspace, surfacing the self-service
//! profile fields from `participant_profiles` (0035) alongside the core
//! `participants` identity. Read-only: this repo owns no mutations — it only
//! projects the join of `participants → workspace_members → participant_profiles`
//! into a [`DirectoryEntry`] list, optionally narrowed by a display-name and/or a
//! job-title substring.
//!
//! The `JOIN workspace_members` is the scope boundary: only members of the given
//! workspace are ever returned, so the directory can never leak a participant who
//! does not belong to the tenant. There is NO migration and NO new id — it reads
//! existing tables only.
//!
//! Purely additive: a NEW [`DirectoryRepo`]; no existing repo is touched. The
//! [`DirectoryEntry`] model lives here (and is re-exported from the crate root)
//! rather than in `aero-common`, since it is a storage-layer projection —
//! mirroring [`Profile`](crate::Profile) and [`SavedSearch`](crate::SavedSearch).

use aero_common::{ParticipantId, WorkspaceId};
use serde::Serialize;
use sqlx::PgPool;

/// One directory row — a workspace member's public-facing identity + profile.
///
/// A storage-layer projection joining a `participants` row to its optional
/// `participant_profiles` side row. The profile-derived fields are `Option`,
/// since a member who never filled in a profile still appears in the directory.
/// `Serialize` so a handler can hand the row straight back as JSON.
#[derive(Debug, Clone, Serialize)]
pub struct DirectoryEntry {
    /// The member's participant id.
    pub participant_id: ParticipantId,
    /// The member's display name (from the core `participants` row).
    pub display_name: String,
    /// The participant kind token (`human` / `bot` / `agent`), verbatim from the
    /// `participants.kind` column.
    pub kind: String,
    /// Job title / role, if the member set one in their profile.
    pub title: Option<String>,
    /// Preferred pronouns, if set.
    pub pronouns: Option<String>,
    /// IANA timezone name, if set.
    pub timezone: Option<String>,
}

/// The columns a [`DirectoryEntry`] is built from, in select order. Shared so the
/// row decoding stays in one place.
const COLUMNS: &str = "p.id, p.display_name, p.kind, pp.title, pp.pronouns, pp.timezone";

/// Largest page the directory will return at once, mirroring the clamp other
/// list endpoints apply so one request can't pull an unbounded result set.
const MAX_LIMIT: i64 = 200;

#[derive(Debug, Clone, sqlx::FromRow)]
struct Row {
    id: uuid::Uuid,
    display_name: String,
    kind: String,
    title: Option<String>,
    pronouns: Option<String>,
    timezone: Option<String>,
}

fn row_to_model(r: Row) -> DirectoryEntry {
    DirectoryEntry {
        participant_id: ParticipantId::from_uuid(r.id),
        display_name: r.display_name,
        kind: r.kind,
        title: r.title,
        pronouns: r.pronouns,
        timezone: r.timezone,
    }
}

/// Repository over the people-directory read model (members + their profiles).
///
/// Cheap to clone — it just wraps a [`PgPool`] (itself an `Arc` internally), so
/// feature modules build one inline via [`DirectoryRepo::new`].
#[derive(Clone)]
#[must_use]
pub struct DirectoryRepo {
    pool: PgPool,
}

impl DirectoryRepo {
    /// Build a repo over the given pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Search the `workspace`'s member directory, ordered by display name.
    ///
    /// Returns every member of `workspace` (the `JOIN workspace_members` scope
    /// boundary), each projected with its optional profile fields. An optional
    /// `query` narrows by a display-name substring (`ILIKE '%query%'`); an
    /// optional `title` narrows by a profile job-title substring. A blank/empty
    /// filter is ignored (treated as absent). `limit` is clamped to
    /// `[1, MAX_LIMIT]` and `offset` floored at `0`, so paging stays bounded.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn search(
        &self,
        workspace: WorkspaceId,
        query: Option<&str>,
        title: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<DirectoryEntry>, sqlx::Error> {
        let limit = limit.clamp(1, MAX_LIMIT);
        let offset = offset.max(0);

        // Treat blank filters as absent so `?q=` / `?title=` don't match nothing.
        let query = query.map(str::trim).filter(|q| !q.is_empty());
        let title = title.map(str::trim).filter(|t| !t.is_empty());

        // Build the statement with positional binds. `$1` is always the
        // workspace; optional filters claim the next slots, then LIMIT/OFFSET.
        let mut sql = format!(
            "SELECT {COLUMNS}
               FROM participants p
               JOIN workspace_members wm ON wm.participant_id = p.id
               LEFT JOIN participant_profiles pp ON pp.participant_id = p.id
              -- Exclude GDPR-tombstoned accounts: erasure soft-deletes the
              -- participant (UPDATE deleted_at + display_name='[deleted]') but does
              -- not remove the workspace_members row (the FK cascade never fires on a
              -- tombstone), so without this filter '[deleted]' rows leak into the
              -- directory. Mirrors ParticipantRepo::search.
              WHERE wm.workspace_id = $1 AND p.deleted_at IS NULL"
        );
        let mut idx = 2;
        if query.is_some() {
            sql.push_str(&format!(" AND p.display_name ILIKE '%'||${idx}||'%'"));
            idx += 1;
        }
        if title.is_some() {
            sql.push_str(&format!(" AND pp.title ILIKE '%'||${idx}||'%'"));
            idx += 1;
        }
        sql.push_str(&format!(
            " ORDER BY p.display_name ASC, p.id ASC LIMIT ${} OFFSET ${}",
            idx,
            idx + 1
        ));

        let mut q = sqlx::query_as::<_, Row>(&sql).bind(workspace.to_uuid());
        if let Some(needle) = query {
            q = q.bind(needle.to_owned());
        }
        if let Some(needle) = title {
            q = q.bind(needle.to_owned());
        }
        let rows = q.bind(limit).bind(offset).fetch_all(&self.pool).await?;
        Ok(rows.into_iter().map(row_to_model).collect())
    }
}

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored directory
/// ```
#[cfg(test)]
mod db_tests {
    use super::*;

    /// The reserved all-zero default workspace, guaranteed to exist by migration
    /// 0006's backfill — reused so the directory rows are well-scoped.
    const DEFAULT_WS: &str = "00000000-0000-0000-0000-000000000000";

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL")
    }

    fn default_ws() -> WorkspaceId {
        WorkspaceId::from_uuid(uuid::Uuid::parse_str(DEFAULT_WS).expect("valid uuid"))
    }

    /// Create a throwaway participant with the given display name, enroll them in
    /// the default workspace, and set a profile title. Returns the new id.
    async fn member_with_title(p: &PgPool, display_name: &str, title: &str) -> ParticipantId {
        let id = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
            .bind(id.to_uuid())
            .bind(display_name)
            .execute(p)
            .await
            .expect("insert participant");
        sqlx::query(
            r"INSERT INTO workspace_members (workspace_id, participant_id, role, joined_at)
               VALUES ($1, $2, 'member', now())
               ON CONFLICT (workspace_id, participant_id) DO NOTHING",
        )
        .bind(default_ws().to_uuid())
        .bind(id.to_uuid())
        .execute(p)
        .await
        .expect("enroll member");
        sqlx::query(
            r"INSERT INTO participant_profiles (participant_id, title) VALUES ($1, $2)
               ON CONFLICT (participant_id) DO UPDATE SET title = EXCLUDED.title",
        )
        .bind(id.to_uuid())
        .bind(title)
        .execute(p)
        .await
        .expect("set profile");
        id
    }

    /// Remove the throwaway rows so reruns stay self-contained.
    async fn cleanup(p: &PgPool, id: ParticipantId) {
        sqlx::query("DELETE FROM participant_profiles WHERE participant_id = $1")
            .bind(id.to_uuid())
            .execute(p)
            .await
            .ok();
        sqlx::query("DELETE FROM workspace_members WHERE participant_id = $1")
            .bind(id.to_uuid())
            .execute(p)
            .await
            .ok();
        sqlx::query("DELETE FROM participants WHERE id = $1")
            .bind(id.to_uuid())
            .execute(p)
            .await
            .ok();
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn search_finds_member_and_title_filter_narrows() {
        let p = pool();
        let repo = DirectoryRepo::new(p.clone());
        let ws = default_ws();
        // A unique token so the assertions don't collide with seed data.
        let tag = format!("dir-{}", ParticipantId::new());
        let eng = member_with_title(&p, &format!("Ada {tag}"), &format!("Engineer {tag}")).await;
        let designer =
            member_with_title(&p, &format!("Grace {tag}"), &format!("Designer {tag}")).await;

        // A name search finds the member, with its profile fields populated.
        let by_name = repo
            .search(ws, Some(&format!("Ada {tag}")), None, 50, 0)
            .await
            .unwrap();
        let found = by_name
            .iter()
            .find(|e| e.participant_id == eng)
            .expect("name search finds the member");
        assert_eq!(found.display_name, format!("Ada {tag}"));
        assert_eq!(found.kind, "human");
        assert_eq!(
            found.title.as_deref(),
            Some(format!("Engineer {tag}").as_str())
        );

        // A title filter narrows: searching the tag across both, then filtering by
        // the engineer's title excludes the designer.
        let both = repo.search(ws, Some(&tag), None, 50, 0).await.unwrap();
        assert!(
            both.iter().any(|e| e.participant_id == eng),
            "engineer in tag set"
        );
        assert!(
            both.iter().any(|e| e.participant_id == designer),
            "designer in tag set"
        );

        let narrowed = repo
            .search(ws, Some(&tag), Some(&format!("Engineer {tag}")), 50, 0)
            .await
            .unwrap();
        assert!(
            narrowed.iter().any(|e| e.participant_id == eng),
            "title filter keeps the engineer"
        );
        assert!(
            !narrowed.iter().any(|e| e.participant_id == designer),
            "title filter excludes the designer"
        );

        cleanup(&p, eng).await;
        cleanup(&p, designer).await;
    }

    /// A GDPR-erased member must NOT appear in the directory. Erasure tombstones
    /// the participant (UPDATE deleted_at, display_name='[deleted]') and leaves the
    /// workspace_members row intact (the FK cascade never fires on a tombstone), so
    /// only the query's `deleted_at IS NULL` filter keeps the '[deleted]' row out.
    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn search_excludes_gdpr_tombstoned_member() {
        let p = pool();
        let repo = DirectoryRepo::new(p.clone());
        let ws = default_ws();
        let tag = format!("dir-del-{}", ParticipantId::new());
        let active = member_with_title(&p, &format!("Active {tag}"), &format!("Eng {tag}")).await;
        let erased = member_with_title(&p, &format!("Erased {tag}"), &format!("Eng {tag}")).await;

        // Erase via the canonical GDPR path (UPDATE tombstone, not a hard delete).
        crate::ParticipantRepo::new(p.clone())
            .delete_participant(erased)
            .await
            .expect("erase");

        // Baseline: the active member is still reachable by name.
        let by_name = repo
            .search(ws, Some(&format!("Active {tag}")), None, 50, 0)
            .await
            .unwrap();
        assert!(
            by_name.iter().any(|e| e.participant_id == active),
            "active member is listed"
        );

        // The erased member must be absent from an UNFILTERED listing (its name is
        // now '[deleted]', so only the deleted_at filter — not the name — excludes it).
        let all = repo.search(ws, None, None, 200, 0).await.unwrap();
        assert!(
            all.iter().any(|e| e.participant_id == active),
            "active member in full listing"
        );
        assert!(
            !all.iter().any(|e| e.participant_id == erased),
            "a GDPR-tombstoned member must NOT leak into the directory"
        );

        cleanup(&p, active).await;
        cleanup(&p, erased).await;
    }
}
