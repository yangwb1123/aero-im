//! Workspace IP / network allowlist (authorized networks) repository.
//!
//! Backs `migrations/0073_workspace_ip_allowlist.sql`. An admin declares the CIDR
//! ranges that are allowed to reach a workspace's data; an EMPTY allowlist means
//! "allow all" (the feature is disabled until at least one CIDR is added). The
//! repo owns the add/remove/list CRUD over the `workspace_ip_allowlist` table.
//!
//! The actual matching is performed by the PURE helpers [`ip_in_cidr`] (one IP vs
//! one CIDR) and [`is_allowed`] (one IP vs a list, with empty-list ⇒ allow). Both
//! hand-roll the v4/v6 prefix comparison over [`std::net`] so there is no heavy
//! CIDR-parsing dependency, and both are fully unit-tested offline (no Postgres).
//! An axum enforcement hook can call [`is_allowed`] with the rows returned by
//! [`IpAllowlistRepo::list`] — see the crate-level wiring notes.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use aero_common::{Error, ParticipantId, WorkspaceId};
use serde::Serialize;
use sqlx::PgPool;

use crate::workspace::authz::assert_effective_admin_in_tx;

/// One authorized-network entry — a `(workspace, cidr)` pair plus an optional note.
///
/// A storage-layer projection of a `workspace_ip_allowlist` row. `Serialize` so a
/// handler can hand the row straight back as JSON; `created_at` renders as RFC 3339.
#[derive(Debug, Clone, Serialize)]
pub struct IpAllowEntry {
    /// Surrogate id of the allowlist row.
    pub id: uuid::Uuid,
    /// The tenant the entry is scoped to.
    pub workspace_id: WorkspaceId,
    /// The authorized network in CIDR notation (e.g. `10.0.0.0/8`, `1.2.3.4/32`).
    pub cidr: String,
    /// Optional human note describing the range (e.g. "HQ office").
    pub note: Option<String>,
    /// When the entry was added (RFC 3339 on the wire).
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: time::OffsetDateTime,
}

/// The columns an [`IpAllowEntry`] is built from, in select order. Shared by every
/// query so the row decoding stays in one place.
const COLUMNS: &str = "id, workspace_id, cidr, note, created_at";

#[derive(Debug, Clone, sqlx::FromRow)]
struct Row {
    id: uuid::Uuid,
    workspace_id: uuid::Uuid,
    cidr: String,
    note: Option<String>,
    created_at: time::OffsetDateTime,
}

fn row_to_model(r: Row) -> IpAllowEntry {
    IpAllowEntry {
        id: r.id,
        workspace_id: WorkspaceId::from_uuid(r.workspace_id),
        cidr: r.cidr,
        note: r.note,
        created_at: r.created_at,
    }
}

/// Repository over the `workspace_ip_allowlist` table (admin authorized-networks).
///
/// Cheap to clone — it just wraps a [`PgPool`] (itself an `Arc` internally), so
/// feature modules build one inline via [`IpAllowlistRepo::new`].
#[derive(Clone)]
#[must_use]
pub struct IpAllowlistRepo {
    pool: PgPool,
}

impl IpAllowlistRepo {
    /// Build a repo over the given pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Add `cidr` (with optional `note`) to `workspace`'s allowlist, returning the
    /// persisted entry. Idempotent on `(workspace, cidr)`: re-adding an existing
    /// range updates only its note (leaving the original `id`/`created_at`), so a
    /// retry is harmless. The caller is responsible for the admin-privilege check
    /// and for validating the CIDR (see [`ip_in_cidr`], which rejects malformed
    /// input at match time).
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the upsert.
    pub async fn add(
        &self,
        workspace: WorkspaceId,
        cidr: &str,
        note: Option<&str>,
    ) -> Result<IpAllowEntry, sqlx::Error> {
        let sql = format!(
            "INSERT INTO workspace_ip_allowlist (workspace_id, cidr, note)
               VALUES ($1, $2, $3)
             ON CONFLICT (workspace_id, cidr) DO UPDATE SET note = EXCLUDED.note
             RETURNING {COLUMNS}"
        );
        let row = sqlx::query_as::<_, Row>(&sql)
            .bind(workspace.to_uuid())
            .bind(cidr)
            .bind(note)
            .fetch_one(&self.pool)
            .await?;
        Ok(row_to_model(row))
    }

    /// Add or update a CIDR under a transactionally current workspace
    /// Owner/Admin decision.
    pub async fn add_authorized(
        &self,
        workspace: WorkspaceId,
        cidr: &str,
        note: Option<&str>,
        actor: ParticipantId,
    ) -> Result<IpAllowEntry, Error> {
        let mut tx = self.pool.begin().await?;
        assert_effective_admin_in_tx(&mut tx, workspace, actor).await?;
        let sql = format!(
            "INSERT INTO workspace_ip_allowlist (workspace_id, cidr, note)
               VALUES ($1, $2, $3)
             ON CONFLICT (workspace_id, cidr) DO UPDATE SET note = EXCLUDED.note
             RETURNING {COLUMNS}"
        );
        let row = sqlx::query_as::<_, Row>(&sql)
            .bind(workspace.to_uuid())
            .bind(cidr)
            .bind(note)
            .fetch_one(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(row_to_model(row))
    }

    /// Remove `cidr` from `workspace`'s allowlist. Returns `true` iff a row was
    /// removed — removing a range that was never added (or a second remove) is a
    /// no-op returning `false`.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the delete.
    pub async fn remove(&self, workspace: WorkspaceId, cidr: &str) -> Result<bool, sqlx::Error> {
        let result =
            sqlx::query("DELETE FROM workspace_ip_allowlist WHERE workspace_id = $1 AND cidr = $2")
                .bind(workspace.to_uuid())
                .bind(cidr)
                .execute(&self.pool)
                .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Remove a CIDR under the same workspace lock as the effective
    /// administrator decision.
    pub async fn remove_authorized(
        &self,
        workspace: WorkspaceId,
        cidr: &str,
        actor: ParticipantId,
    ) -> Result<bool, Error> {
        let mut tx = self.pool.begin().await?;
        assert_effective_admin_in_tx(&mut tx, workspace, actor).await?;
        let removed =
            sqlx::query("DELETE FROM workspace_ip_allowlist WHERE workspace_id = $1 AND cidr = $2")
                .bind(workspace.to_uuid())
                .bind(cidr)
                .execute(&mut *tx)
                .await?
                .rows_affected()
                > 0;
        tx.commit().await?;
        Ok(removed)
    }

    /// List the authorized networks for `workspace`, oldest first.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn list(&self, workspace: WorkspaceId) -> Result<Vec<IpAllowEntry>, sqlx::Error> {
        let sql = format!(
            "SELECT {COLUMNS}
               FROM workspace_ip_allowlist
              WHERE workspace_id = $1
              ORDER BY created_at ASC, id ASC"
        );
        let rows = sqlx::query_as::<_, Row>(&sql)
            .bind(workspace.to_uuid())
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(row_to_model).collect())
    }

    /// Convenience: the raw CIDR strings for `workspace`, ready to hand to
    /// [`is_allowed`] from an enforcement hook.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn cidrs(&self, workspace: WorkspaceId) -> Result<Vec<String>, sqlx::Error> {
        Ok(self
            .list(workspace)
            .await?
            .into_iter()
            .map(|e| e.cidr)
            .collect())
    }

    /// Return every configured authorized network for workspaces the participant
    /// currently belongs to.
    ///
    /// The result is ordered by workspace and allowlist row so callers can group
    /// it without another query. Workspaces with an empty allowlist are absent:
    /// they are unrestricted by definition. This single JOIN is the hot-path
    /// primitive used by the gateway for global REST and WebSocket enforcement;
    /// it avoids a query per workspace and, importantly, also covers resource
    /// routes whose URL contains a room/message id rather than a workspace id.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn configured_for_participant(
        &self,
        participant: ParticipantId,
    ) -> Result<Vec<(WorkspaceId, String)>, sqlx::Error> {
        let rows = sqlx::query_as::<_, (uuid::Uuid, String)>(
            r"SELECT a.workspace_id, a.cidr
                FROM workspace_ip_allowlist a
                JOIN workspace_members m
                  ON m.workspace_id = a.workspace_id
               WHERE m.participant_id = $1
               ORDER BY a.workspace_id, a.created_at, a.id",
        )
        .bind(participant.to_uuid())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(workspace, cidr)| (WorkspaceId::from_uuid(workspace), cidr))
            .collect())
    }
}

/// Whether `ip` falls inside `cidr` (e.g. `"10.0.0.0/8"`, `"1.2.3.4/32"`,
/// `"2001:db8::/32"`).
///
/// Hand-rolled v4/v6 prefix match over [`std::net`] — no external CIDR crate. The
/// address family must match: a v4 `ip` never matches a v6 `cidr` and vice versa.
/// A malformed `cidr` (missing `/`, non-numeric or out-of-range prefix length,
/// unparseable network address, family mismatch) returns `false` — it never
/// panics and never matches, so bad input fails closed.
#[must_use]
pub fn ip_in_cidr(ip: IpAddr, cidr: &str) -> bool {
    let Some((net_str, len_str)) = cidr.trim().split_once('/') else {
        return false;
    };
    let Ok(prefix_len) = len_str.trim().parse::<u32>() else {
        return false;
    };
    let Ok(net) = net_str.trim().parse::<IpAddr>() else {
        return false;
    };
    match (ip, net) {
        (IpAddr::V4(ip4), IpAddr::V4(net4)) => {
            if prefix_len > 32 {
                return false;
            }
            v4_in_prefix(ip4, net4, prefix_len)
        }
        (IpAddr::V6(ip6), IpAddr::V6(net6)) => {
            if prefix_len > 128 {
                return false;
            }
            v6_in_prefix(ip6, net6, prefix_len)
        }
        // Mixed address families never match.
        _ => false,
    }
}

/// `true` iff `ip` and `net` share their top `prefix_len` bits (IPv4). A
/// `prefix_len` of 0 matches everything; 32 requires an exact match.
fn v4_in_prefix(ip: Ipv4Addr, net: Ipv4Addr, prefix_len: u32) -> bool {
    let ip_bits = u32::from(ip);
    let net_bits = u32::from(net);
    if prefix_len == 0 {
        return true;
    }
    // `prefix_len` is in 1..=32 here, so the shift is well-defined.
    let mask: u32 = u32::MAX << (32 - prefix_len);
    (ip_bits & mask) == (net_bits & mask)
}

/// `true` iff `ip` and `net` share their top `prefix_len` bits (IPv6). A
/// `prefix_len` of 0 matches everything; 128 requires an exact match.
fn v6_in_prefix(ip: Ipv6Addr, net: Ipv6Addr, prefix_len: u32) -> bool {
    let ip_bits = u128::from(ip);
    let net_bits = u128::from(net);
    if prefix_len == 0 {
        return true;
    }
    // `prefix_len` is in 1..=128 here, so the shift is well-defined.
    let mask: u128 = u128::MAX << (128 - prefix_len);
    (ip_bits & mask) == (net_bits & mask)
}

/// Whether `ip` is allowed given a workspace's allowlist `cidrs`.
///
/// An EMPTY list means the allowlist is DISABLED ⇒ allow-all (`true`). Otherwise
/// `ip` must match at least one well-formed CIDR. Malformed entries are ignored
/// (they simply never match, via [`ip_in_cidr`]).
#[must_use]
pub fn is_allowed(ip: IpAddr, cidrs: &[String]) -> bool {
    cidrs.is_empty() || cidrs.iter().any(|c| ip_in_cidr(ip, c))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v4(s: &str) -> IpAddr {
        s.parse().expect("valid v4")
    }

    fn v6(s: &str) -> IpAddr {
        s.parse().expect("valid v6")
    }

    #[test]
    fn v4_in_and_out_of_range() {
        // /8 covers the whole 10.0.0.0/8 block.
        assert!(ip_in_cidr(v4("10.1.2.3"), "10.0.0.0/8"));
        assert!(ip_in_cidr(v4("10.255.255.255"), "10.0.0.0/8"));
        // 11.x is outside 10.0.0.0/8.
        assert!(!ip_in_cidr(v4("11.0.0.1"), "10.0.0.0/8"));
        // /24 is tighter.
        assert!(ip_in_cidr(v4("192.168.1.42"), "192.168.1.0/24"));
        assert!(!ip_in_cidr(v4("192.168.2.42"), "192.168.1.0/24"));
    }

    #[test]
    fn v4_slash_32_is_exact_host() {
        assert!(ip_in_cidr(v4("1.2.3.4"), "1.2.3.4/32"));
        assert!(!ip_in_cidr(v4("1.2.3.5"), "1.2.3.4/32"));
    }

    #[test]
    fn v4_slash_zero_matches_everything() {
        assert!(ip_in_cidr(v4("8.8.8.8"), "0.0.0.0/0"));
        assert!(ip_in_cidr(v4("203.0.113.7"), "0.0.0.0/0"));
    }

    #[test]
    fn v6_in_and_out_of_range() {
        assert!(ip_in_cidr(v6("2001:db8::1"), "2001:db8::/32"));
        assert!(ip_in_cidr(v6("2001:db8:dead:beef::1"), "2001:db8::/32"));
        assert!(!ip_in_cidr(v6("2001:db9::1"), "2001:db8::/32"));
        // /128 exact host.
        assert!(ip_in_cidr(v6("::1"), "::1/128"));
        assert!(!ip_in_cidr(v6("::2"), "::1/128"));
    }

    #[test]
    fn mixed_families_never_match() {
        // v4 ip vs v6 cidr and vice versa.
        assert!(!ip_in_cidr(v4("10.0.0.1"), "2001:db8::/32"));
        assert!(!ip_in_cidr(v6("2001:db8::1"), "10.0.0.0/8"));
    }

    #[test]
    fn bad_cidr_returns_false() {
        // No slash.
        assert!(!ip_in_cidr(v4("10.0.0.1"), "10.0.0.0"));
        // Non-numeric prefix.
        assert!(!ip_in_cidr(v4("10.0.0.1"), "10.0.0.0/eight"));
        // Out-of-range prefix length for the family.
        assert!(!ip_in_cidr(v4("10.0.0.1"), "10.0.0.0/33"));
        assert!(!ip_in_cidr(v6("::1"), "::/129"));
        // Unparseable network address.
        assert!(!ip_in_cidr(v4("10.0.0.1"), "not-an-ip/8"));
        // Empty string.
        assert!(!ip_in_cidr(v4("10.0.0.1"), ""));
    }

    #[test]
    fn is_allowed_empty_list_allows_all() {
        // Disabled allowlist ⇒ allow-all.
        assert!(is_allowed(v4("203.0.113.7"), &[]));
        assert!(is_allowed(v6("2001:db8::1"), &[]));
    }

    #[test]
    fn is_allowed_requires_a_match_when_non_empty() {
        let cidrs = vec!["10.0.0.0/8".to_string(), "192.168.1.0/24".to_string()];
        assert!(is_allowed(v4("10.9.9.9"), &cidrs));
        assert!(is_allowed(v4("192.168.1.5"), &cidrs));
        // Outside every range ⇒ denied.
        assert!(!is_allowed(v4("172.16.0.1"), &cidrs));
    }

    #[test]
    fn is_allowed_ignores_malformed_entries() {
        // A single bad entry never matches and never panics; a good one still does.
        let cidrs = vec!["garbage".to_string(), "10.0.0.0/8".to_string()];
        assert!(is_allowed(v4("10.0.0.1"), &cidrs));
        // With only a bad entry, nothing matches (and the list is non-empty, so it
        // is NOT treated as disabled).
        let only_bad = vec!["garbage".to_string()];
        assert!(!is_allowed(v4("10.0.0.1"), &only_bad));
    }
}

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored ip_allowlist
/// ```
#[cfg(test)]
mod db_tests {
    use super::*;
    use aero_common::ParticipantId;

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL")
    }

    /// Create a throwaway participant so the workspace's `created_by` FK resolves.
    async fn mk_participant(p: &PgPool) -> ParticipantId {
        let id = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
            .bind(id.to_uuid())
            .bind(format!("ip-allow-{id}"))
            .execute(p)
            .await
            .expect("insert participant");
        id
    }

    /// Create a throwaway workspace so the allowlist rows are isolated from other
    /// tests running in parallel against the shared DB.
    async fn fresh_ws(p: &PgPool, creator: ParticipantId) -> WorkspaceId {
        let id = WorkspaceId::new();
        let mut tx = p.begin().await.expect("begin workspace fixture");
        sqlx::query(
            "INSERT INTO workspaces (id, name, slug, created_by, created_at)
             VALUES ($1, $2, $3, $4, now())",
        )
        .bind(id.to_uuid())
        .bind(format!("ip-allow-ws-{id}"))
        .bind(format!("ipa-{id}"))
        .bind(creator.to_uuid())
        .execute(&mut *tx)
        .await
        .expect("insert workspace");
        sqlx::query(
            "INSERT INTO workspace_members (workspace_id, participant_id, role)
             VALUES ($1, $2, 'owner')",
        )
        .bind(id.to_uuid())
        .bind(creator.to_uuid())
        .execute(&mut *tx)
        .await
        .expect("insert workspace owner");
        tx.commit().await.expect("commit workspace fixture");
        id
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn add_list_remove_round_trip() {
        let p = pool();
        let repo = IpAllowlistRepo::new(p.clone());
        let creator = mk_participant(&p).await;
        let ws = fresh_ws(&p, creator).await;

        // Empty to start ⇒ allow-all.
        assert!(repo.list(ws).await.unwrap().is_empty());
        assert!(is_allowed(
            "203.0.113.7".parse().unwrap(),
            &repo.cidrs(ws).await.unwrap()
        ));

        // Add two ranges; one with a note.
        let added = repo.add(ws, "10.0.0.0/8", Some("HQ")).await.unwrap();
        assert_eq!(added.cidr, "10.0.0.0/8");
        assert_eq!(added.note.as_deref(), Some("HQ"));
        repo.add(ws, "192.168.1.0/24", None).await.unwrap();
        let configured = repo.configured_for_participant(creator).await.unwrap();
        assert_eq!(
            configured
                .iter()
                .filter(|(configured_ws, _)| *configured_ws == ws)
                .count(),
            2,
            "the participant-wide lookup returns every configured CIDR"
        );
        let outsider = mk_participant(&p).await;
        assert!(
            repo.configured_for_participant(outsider)
                .await
                .unwrap()
                .is_empty(),
            "non-members cannot inherit another workspace's network policy"
        );

        // Idempotent re-add updates only the note, keeps the id.
        let re = repo
            .add(ws, "10.0.0.0/8", Some("HQ-renamed"))
            .await
            .unwrap();
        assert_eq!(re.id, added.id, "re-add keeps the same row id");
        assert_eq!(re.note.as_deref(), Some("HQ-renamed"));

        // List returns both, oldest first.
        let listed = repo.list(ws).await.unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].cidr, "10.0.0.0/8");
        assert_eq!(listed[1].cidr, "192.168.1.0/24");

        // Enforcement check against the stored CIDRs.
        let cidrs = repo.cidrs(ws).await.unwrap();
        assert!(is_allowed("10.5.5.5".parse().unwrap(), &cidrs));
        assert!(is_allowed("192.168.1.42".parse().unwrap(), &cidrs));
        assert!(!is_allowed("172.16.0.1".parse().unwrap(), &cidrs));

        // Remove → true once, then a no-op false; list shrinks.
        assert!(repo.remove(ws, "10.0.0.0/8").await.unwrap(), "first remove");
        assert!(
            !repo.remove(ws, "10.0.0.0/8").await.unwrap(),
            "second remove no-op"
        );
        let after = repo.list(ws).await.unwrap();
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].cidr, "192.168.1.0/24");

        // Cleanup (the workspace CASCADE would also drop these on ws delete).
        sqlx::query("DELETE FROM workspace_ip_allowlist WHERE workspace_id = $1")
            .bind(ws.to_uuid())
            .execute(&p)
            .await
            .ok();
    }
}
