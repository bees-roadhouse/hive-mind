//! The grant predicate's one caller, and the writers on the grants table.

use std::fmt;
use std::time::Duration;

use chrono::{DateTime, Utc};
use hive_db::{Connection, Db, Transaction, query};
use hive_identity::{Credential, Owner, PrincipalKind};
use uuid::Uuid;

use crate::predicate::{self, Args};
use crate::{Result, StoreError};

/// What a grant is written against (D18.1). Allowlist only ... there is no deny
/// kind, because deny rows plus deny-on-absence is two policies that eventually
/// disagree.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SubjectKind {
    Install,
    Tool,
    Route,
    Collection,
    Entity,
    /// A whole chat thread. It resolves through `subject_owners` like every
    /// other kind, so nothing above the data layer learns a new shape.
    Conversation,
}

impl SubjectKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SubjectKind::Install => "install",
            SubjectKind::Tool => "tool",
            SubjectKind::Route => "route",
            SubjectKind::Collection => "collection",
            SubjectKind::Entity => "entity",
            SubjectKind::Conversation => "conversation",
        }
    }

    pub fn parse(s: &str) -> Option<SubjectKind> {
        Some(match s {
            "install" => SubjectKind::Install,
            "tool" => SubjectKind::Tool,
            "route" => SubjectKind::Route,
            "collection" => SubjectKind::Collection,
            "entity" => SubjectKind::Entity,
            "conversation" => SubjectKind::Conversation,
            _ => return None,
        })
    }
}

impl fmt::Display for SubjectKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Access levels. Write implies read; call gates tools and routes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Access {
    Read,
    Write,
    Call,
}

impl Access {
    pub fn as_str(self) -> &'static str {
        match self {
            Access::Read => "read",
            Access::Write => "write",
            Access::Call => "call",
        }
    }
}

impl fmt::Display for Access {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Distinguishes a grant somebody wrote, one the materializer derived, and one
/// policy produced (D18.2, D18.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum GrantSource {
    Direct,
    Inherited,
    Override,
}

impl GrantSource {
    pub fn as_str(self) -> &'static str {
        match self {
            GrantSource::Direct => "direct",
            GrantSource::Inherited => "inherited",
            GrantSource::Override => "override",
        }
    }
}

/// Why access was allowed. `None` is deny. It is not a boolean because D18.2
/// requires auditing accesses that succeeded ONLY through an override, and a
/// boolean cannot say which branch fired.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Reason {
    Owner,
    Grant,
    Org,
    Override,
    /// D33: the principal was allowed AND the asking install holds a grant for
    /// this collection. It is reported in place of the principal's own reason
    /// because the install grant is the narrower of the two, so provenance
    /// names the row a person can revoke to shut one app out without touching
    /// their own access.
    InstallGrant,
}

impl Reason {
    pub fn as_str(self) -> &'static str {
        match self {
            Reason::Owner => "owner",
            Reason::Grant => "grant",
            Reason::Org => "org_grant",
            Reason::Override => "override",
            Reason::InstallGrant => "install_grant",
        }
    }

    pub fn parse(s: &str) -> Option<Reason> {
        Some(match s {
            "owner" => Reason::Owner,
            "grant" => Reason::Grant,
            "org_grant" => Reason::Org,
            "override" => Reason::Override,
            "install_grant" => Reason::InstallGrant,
            // A reason the predicate returns and this enum does not know is a
            // deny here, which is fail-closed but silent: an allowed access
            // would be reported as denied and look like a policy bug. The
            // predicate text and this match are edited together.
            _ => return None,
        })
    }
}

impl fmt::Display for Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Identifies what a grant is written against. `name` is `None` for install,
/// entity and conversation subjects; for tool, route and collection, `id` is
/// the install id and `name` qualifies within it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Subject {
    pub kind: SubjectKind,
    pub id: Uuid,
    pub name: Option<String>,
}

impl Subject {
    pub fn new(kind: SubjectKind, id: Uuid) -> Subject {
        Subject {
            kind,
            id,
            name: None,
        }
    }

    pub fn named(kind: SubjectKind, id: Uuid, name: impl Into<String>) -> Subject {
        Subject {
            kind,
            id,
            name: Some(name.into()),
        }
    }

    pub fn install(id: Uuid) -> Subject {
        Subject::new(SubjectKind::Install, id)
    }

    pub fn entity(id: Uuid) -> Subject {
        Subject::new(SubjectKind::Entity, id)
    }

    pub fn conversation(id: Uuid) -> Subject {
        Subject::new(SubjectKind::Conversation, id)
    }

    pub fn collection(install: Uuid, name: impl Into<String>) -> Subject {
        Subject::named(SubjectKind::Collection, install, name)
    }

    pub fn tool(install: Uuid, name: impl Into<String>) -> Subject {
        Subject::named(SubjectKind::Tool, install, name)
    }

    fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }
}

/// Which install a call is being made THROUGH, when it is being made through
/// one at all (D33).
///
/// This is the dimension the predicate was missing: a `collection` subject
/// resolves its owner to the INSTALL's owner, so without this the predicate
/// cannot tell one of a principal's apps from another and every collection
/// grant it derives decides nothing (invariant 14).
///
/// `None` means the call is not being made through an install ... a person on
/// the HTTP surface reading their own data. It does NOT mean "unrestricted",
/// and the difference matters: SQL cannot tell an argument that was omitted
/// from one that was deliberately absent, so `authorize` REFUSES a collection
/// subject outright and `authorize_collection` is the only way to decide one.
/// That is a runtime refusal with a test behind it, not a type-level barrier,
/// and it is written down as such rather than dressed up as one.
/// **Nothing in the platform writes an install grant yet, and that is
/// intended.** `write_grant` binds `target_kind` and `target_id` and no
/// `target_install_id`, so it cannot produce one ... and the
/// `grants_target_shape` CHECK would refuse it if it tried. The registry will
/// write them when it derives an app's manifest `uses` at activation, which is
/// the rest of #86.
///
/// Until then a cross-install collection read denies, always, and **that
/// denial is the feature working**. This note exists because the failure looks
/// identical to a bug: an app declares what it needs, the install activates,
/// and every read is refused with nothing in the logs to say why. Someone will
/// lose an afternoon to it otherwise.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ActingInstall(pub Uuid);

/// The point check's text, with every argument a placeholder: subject kind,
/// id, name, principal kind, id, actor, access, now, acting install.
pub(crate) fn point_sql() -> String {
    predicate::decision(&Args {
        subject_kind: "?1",
        subject_id: "?2",
        subject_name: "?3",
        principal_kind: "?4",
        principal_id: "?5",
        actor_id: "?6",
        access: "?7",
        now: "?8",
        acting_install: "?9",
    })
}

/// Answers "may this actor do this" and is the only thing in the platform
/// allowed to. It holds no policy of its own: every decision comes from the
/// predicate text in `predicate.rs`, which is the SQL function migration one
/// used to install.
///
/// Two properties are structural rather than conventional, because both were
/// lost once when they were conventions:
///
/// - No method takes an owner. An earlier signature accepted one and compared
///   it to the credential's principal, which meant every caller composed half
///   the access check; passing your own principal returned "owner" for any row
///   in the database.
/// - There is no exported non-auditing entry point. D18.2 requires that an
///   access which succeeded only through an override writes an audit row, and
///   "use authorize on a real access path" is what a convention looks like when
///   it loses: the set-read form skipped the audit entirely.
///
/// Reads go through the connection each method is handed, so a caller inside a
/// transaction sees its own writes. Audit rows land in the audit file, on a
/// connection of its own, on purpose: with one writer per file a second
/// connection on the SAME file would wait on the caller's write lock, so the
/// evidence has a file to itself (D38 §3).
#[derive(Clone)]
pub struct Guard {
    audit: Db,
}

impl Guard {
    pub(crate) fn new(audit: Db) -> Guard {
        Guard { audit }
    }

    /// The single call every method here funnels through. Nothing outside this
    /// module and `events.rs` may reference the predicate or the grants table.
    pub(crate) async fn decision(
        &self,
        db: &Connection,
        cred: &Credential,
        subj: &Subject,
        access: Access,
        acting: Option<ActingInstall>,
    ) -> Result<(Option<Reason>, Option<Uuid>)> {
        let row = query(&point_sql())
            .bind(subj.kind.as_str())
            .bind(subj.id)
            .bind(subj.name())
            .bind(cred.principal_kind.as_str())
            .bind(cred.principal_id)
            .bind(cred.actor_id)
            .bind(access.as_str())
            .bind(hive_db::now())
            .bind(acting.map(|a| a.0))
            .fetch_one(db)
            .await
            .map_err(|e| StoreError::db("access_decision", e))?;
        let reason: Option<String> = row.get("reason");
        let grant_id: Option<Uuid> = row.get("grant_id");
        Ok((reason.as_deref().and_then(Reason::parse), grant_id))
    }

    /// The point check. Returns why access was allowed, or `Denied`, and audits
    /// an override before returning.
    pub async fn authorize(
        &self,
        db: &Connection,
        cred: &Credential,
        subj: &Subject,
        access: Access,
        note: &str,
    ) -> Result<Reason> {
        // D33. A collection decision depends on which install is asking, and
        // this signature has nowhere to say. Refusing here is what stops the
        // dimension being dropped by a caller who simply did not know about
        // it: the alternative default, "no acting install means no
        // restriction", is the fail-open one and it is invisible at the call
        // site. Every collection goes through `authorize_collection`.
        if subj.kind == SubjectKind::Collection {
            return Err(StoreError::Other(
                "collection access must go through Guard::authorize_collection, \
                 which requires the acting install (D33)"
                    .into(),
            ));
        }
        self.decide_and_audit(db, cred, subj, access, None, note)
            .await
    }

    /// The collection decision made THROUGH an install, which is every guest
    /// call (D33).
    ///
    /// This takes an `ActingInstall`, not an `Option`. An earlier version took
    /// the option, and the safe call and the fail-open call were then the same
    /// keystrokes apart: a guest path that passed `None` got the old
    /// owner-branch behaviour silently, which is the exact bug D33 exists to
    /// close, reachable by autocomplete. The person-on-the-HTTP-surface case
    /// has its own name below, so the dangerous shape is not merely
    /// discouraged ... it cannot be written here at all.
    pub async fn authorize_collection(
        &self,
        db: &Connection,
        cred: &Credential,
        subj: &Subject,
        acting: ActingInstall,
        access: Access,
        note: &str,
    ) -> Result<Reason> {
        self.collection_decision(db, cred, subj, Some(acting), access, note)
            .await
    }

    /// The collection decision made by a person reaching their own data
    /// through no install at all ... the HTTP surface, not a guest.
    ///
    /// Separately named on purpose. This is the one call in the codebase that
    /// legitimately skips D33's dimension, and it should be greppable and
    /// obvious in review rather than looking identical to the guest path with
    /// one argument different.
    pub async fn authorize_collection_as_person(
        &self,
        db: &Connection,
        cred: &Credential,
        subj: &Subject,
        access: Access,
        note: &str,
    ) -> Result<Reason> {
        self.collection_decision(db, cred, subj, None, access, note)
            .await
    }

    async fn collection_decision(
        &self,
        db: &Connection,
        cred: &Credential,
        subj: &Subject,
        acting: Option<ActingInstall>,
        access: Access,
        note: &str,
    ) -> Result<Reason> {
        // The mirror of the refusal in `authorize`, so the pair is exhaustive:
        // a collection can only be decided here, and only a collection is.
        // Without this the method would quietly accept a tool or entity subject
        // and silently ignore `acting`, which is a worse failure than either
        // refusal because it looks like it did something.
        if subj.kind != SubjectKind::Collection {
            return Err(StoreError::Other(format!(
                "authorize_collection got a {} subject; use Guard::authorize",
                subj.kind
            )));
        }
        self.decide_and_audit(db, cred, subj, access, acting, note)
            .await
    }

    /// What both of the above are: one decision, one audit obligation. Kept
    /// private so there is still no exported entry point that skips the audit.
    async fn decide_and_audit(
        &self,
        db: &Connection,
        cred: &Credential,
        subj: &Subject,
        access: Access,
        acting: Option<ActingInstall>,
        note: &str,
    ) -> Result<Reason> {
        let (reason, grant_id) = self.decision(db, cred, subj, access, acting).await?;
        let reason = reason.ok_or(StoreError::Denied)?;
        if reason == Reason::Override {
            // Refuse the access rather than let it happen unaudited.
            // Visibility is what makes the power acceptable.
            self.record_override(db, cred, subj, access, grant_id, note)
                .await
                .map_err(|e| {
                    StoreError::Other(format!("override audit failed, access refused: {e}"))
                })?;
        }
        Ok(reason)
    }

    /// `authorize` reduced to a boolean, for call sites that do not care which
    /// branch fired. It still audits.
    pub async fn allowed(
        &self,
        db: &Connection,
        cred: &Credential,
        subj: &Subject,
        access: Access,
    ) -> Result<bool> {
        match self.authorize(db, cred, subj, access, "").await {
            Ok(_) => Ok(true),
            Err(StoreError::Denied) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Writes the audit row in the audit file, outside whatever transaction
    /// the caller is running, and fails loudly if it wrote nothing.
    ///
    /// The owner comes from `subject_owners`, read on the caller's connection
    /// so it is the same fact the decision just resolved, for the same reason
    /// the predicate resolves it: an audit row naming an owner the caller
    /// supplied would record the caller's belief rather than the fact. A
    /// subject that resolves to no owner cannot have reached 'override', so
    /// that reads as an error rather than as an unowned audit row.
    async fn record_override(
        &self,
        db: &Connection,
        cred: &Credential,
        subj: &Subject,
        access: Access,
        grant_id: Option<Uuid>,
        note: &str,
    ) -> Result<()> {
        let owner = query(
            "SELECT owner_kind, owner_id FROM subject_owners
              WHERE subject_kind = ?1 AND subject_id = ?2",
        )
        .bind(subj.kind.as_str())
        .bind(subj.id)
        .fetch_optional(db)
        .await
        .map_err(|e| StoreError::db("resolve audit owner", e))?
        .ok_or_else(|| StoreError::Other("override audit: subject has no owner".into()))?;
        let owner_kind: String = owner.get("owner_kind");
        let owner_id: Uuid = owner.get("owner_id");
        let c = self
            .audit
            .conn()
            .await
            .map_err(|e| StoreError::db("audit connection", e))?;
        let n = query(
            "INSERT INTO grant_override_audit (
                 grant_id, actor_id, principal_kind, principal_id,
                 subject_kind, subject_id, subject_name,
                 owner_kind, owner_id, access, reason, occurred_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        )
        .bind(grant_id)
        .bind(cred.actor_id)
        .bind(cred.principal_kind.as_str())
        .bind(cred.principal_id)
        .bind(subj.kind.as_str())
        .bind(subj.id)
        .bind(subj.name())
        .bind(owner_kind)
        .bind(owner_id)
        .bind(access.as_str())
        .bind(note)
        .bind(hive_db::now())
        .execute(&c)
        .await
        .map_err(|e| StoreError::db("write override audit", e))?;
        if n == 0 {
            return Err(StoreError::Other("override audit wrote no row".into()));
        }
        Ok(())
    }

    /// The allowlist-only rule (D18.1) for a named kind under an install: an
    /// install grant with no allowlist implies the full set; with one, exactly
    /// those names.
    async fn named_reason(
        &self,
        db: &Connection,
        cred: &Credential,
        kind: SubjectKind,
        install_id: Uuid,
        name: &str,
        note: &str,
    ) -> Result<Option<Reason>> {
        let probe = predicate::has_allowlist("?1", "?2", "?3", "?4", "?5");
        let has_allowlist: bool = query(&probe)
            .bind(kind.as_str())
            .bind(install_id)
            .bind(cred.principal_kind.as_str())
            .bind(cred.principal_id)
            .bind(hive_db::now())
            .fetch_scalar(db)
            .await
            .map_err(|e| StoreError::db(format!("{kind} allowlist probe"), e))?;
        let subj = if has_allowlist {
            Subject::named(kind, install_id, name)
        } else {
            Subject::install(install_id)
        };
        // A tool or route subject, never a collection: D33's dimension does
        // not apply and `None` here is the fact, not a default.
        let (reason, grant_id) = self.decision(db, cred, &subj, Access::Call, None).await?;
        let Some(r) = reason else {
            return Ok(None);
        };
        if r == Reason::Override {
            let audited = Subject::named(kind, install_id, name);
            self.record_override(db, cred, &audited, Access::Call, grant_id, note)
                .await
                .map_err(|e| {
                    StoreError::Other(format!("override audit failed, access refused: {e}"))
                })?;
        }
        Ok(Some(r))
    }

    /// Applies the allowlist-only rule (D18.1): an install grant with no tool
    /// allowlist implies the full tool set; with one, exactly those tools.
    pub async fn tool_reason(
        &self,
        db: &Connection,
        cred: &Credential,
        install_id: Uuid,
        tool: &str,
    ) -> Result<Option<Reason>> {
        self.named_reason(db, cred, SubjectKind::Tool, install_id, tool, "tool call")
            .await
    }

    /// The route twin of `tool_reason`: an install grant with no route
    /// allowlist implies every route the app mounts; with one, exactly those.
    /// A route is named `"<METHOD> <path template>"`, which the manifest keeps
    /// unique per app.
    pub async fn route_reason(
        &self,
        db: &Connection,
        cred: &Credential,
        install_id: Uuid,
        route: &str,
    ) -> Result<Option<Reason>> {
        self.named_reason(
            db,
            cred,
            SubjectKind::Route,
            install_id,
            route,
            "route call",
        )
        .await
    }

    /// The set-read form, and it carries the same audit obligation as the point
    /// check.
    ///
    /// The earlier version returned override-only rows and wrote no audit rows
    /// at all, because the obligation lived on `authorize` rather than on the
    /// predicate. That made it optional for anyone who reached for this method,
    /// which is every list, search and graph query there will ever be.
    pub async fn visible_entity_ids(
        &self,
        db: &Connection,
        cred: &Credential,
        access: Access,
        kind: &str,
        limit: i64,
    ) -> Result<Vec<Uuid>> {
        let reason = predicate::reason(&Args {
            subject_kind: "'entity'",
            subject_id: "e.id",
            subject_name: "NULL",
            principal_kind: "?2",
            principal_id: "?3",
            actor_id: "?4",
            access: "?5",
            now: "?6",
            acting_install: "NULL",
        });
        let rows = query(&format!(
            "SELECT e.id, {reason} AS reason
               FROM entities e
              WHERE e.deleted_at IS NULL
                AND (?1 = '' OR e.kind = ?1)
                AND reason IS NOT NULL
              ORDER BY e.created_at DESC
              LIMIT ?7"
        ))
        .bind(kind)
        .bind(cred.principal_kind.as_str())
        .bind(cred.principal_id)
        .bind(cred.actor_id)
        .bind(access.as_str())
        .bind(hive_db::now())
        .bind(limit)
        .fetch_all(db)
        .await
        .map_err(|e| StoreError::db("visible entities", e))?;
        self.visible_ids(db, cred, access, SubjectKind::Entity, rows)
            .await
    }

    /// The same question for chat threads, most recently active first. Archived
    /// threads are not listed, and archiving is not a grant question: it is the
    /// owner putting a thread away, and a stranger's grant does not unpack it.
    pub async fn visible_conversation_ids(
        &self,
        db: &Connection,
        cred: &Credential,
        access: Access,
        limit: i64,
    ) -> Result<Vec<Uuid>> {
        let reason = predicate::reason(&Args {
            subject_kind: "'conversation'",
            subject_id: "c.id",
            subject_name: "NULL",
            principal_kind: "?1",
            principal_id: "?2",
            actor_id: "?3",
            access: "?4",
            now: "?5",
            acting_install: "NULL",
        });
        let rows = query(&format!(
            "SELECT c.id, {reason} AS reason
               FROM conversations c
              WHERE c.archived_at IS NULL
                AND reason IS NOT NULL
              ORDER BY c.updated_at DESC
              LIMIT ?6"
        ))
        .bind(cred.principal_kind.as_str())
        .bind(cred.principal_id)
        .bind(cred.actor_id)
        .bind(access.as_str())
        .bind(hive_db::now())
        .bind(limit)
        .fetch_all(db)
        .await
        .map_err(|e| StoreError::db("visible conversations", e))?;
        self.visible_ids(db, cred, access, SubjectKind::Conversation, rows)
            .await
    }

    /// Audits every override in a list result before any id leaves.
    ///
    /// One implementation for every list, because the audit obligation is the
    /// part a second copy forgets: the query is easy to get right and the audit
    /// is the thing that was missing the first time.
    async fn visible_ids(
        &self,
        db: &Connection,
        cred: &Credential,
        access: Access,
        kind: SubjectKind,
        rows: Vec<hive_db::Row>,
    ) -> Result<Vec<Uuid>> {
        let mut ids = Vec::with_capacity(rows.len());
        let mut overrides = Vec::new();
        for row in rows {
            let id: Uuid = row.get("id");
            let reason: String = row.get("reason");
            ids.push(id);
            if Reason::parse(&reason) == Some(Reason::Override) {
                overrides.push(id);
            }
        }
        // Audit before returning. If the evidence cannot be written, the rows
        // do not leave this function.
        for id in overrides {
            let subj = Subject::new(kind, id);
            // `kind` is Entity or Conversation on every path that reaches here;
            // neither is install-scoped, so there is no asking install to name.
            let (_, grant_id) = self.decision(db, cred, &subj, access, None).await?;
            self.record_override(db, cred, &subj, access, grant_id, "list")
                .await
                .map_err(|e| {
                    StoreError::Other(format!(
                        "override audit failed, {} row(s) withheld: {e}",
                        ids.len()
                    ))
                })?;
        }
        Ok(ids)
    }
}

/// One grant to write. The database enforces who may write it
/// (`grants_issue_policy`), so a caller cannot widen anything by constructing
/// this carefully.
#[derive(Clone, Debug)]
pub struct GrantSpec {
    pub subject: Subject,
    pub target: Owner,
    pub access: Access,
    pub source: GrantSource,
    /// Set only by the materializer.
    pub inherited_from: Option<Uuid>,
    pub by: Credential,
    pub reason: String,
    pub expires_at: Option<DateTime<Utc>>,
}

impl GrantSpec {
    pub fn direct(subject: Subject, target: Owner, access: Access, by: Credential) -> GrantSpec {
        GrantSpec {
            subject,
            target,
            access,
            source: GrantSource::Direct,
            inherited_from: None,
            by,
            reason: String::new(),
            expires_at: None,
        }
    }
}

/// Inserts one grant. Returns the new id.
pub async fn write_grant(db: &Connection, spec: &GrantSpec) -> Result<Uuid> {
    query(
        "INSERT INTO grants (
             id, subject_kind, subject_id, subject_name,
             target_kind, target_id, access, source, inherited_from,
             granted_by_actor, granted_by_principal_kind, granted_by_principal_id,
             reason, expires_at, created_at)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)
         RETURNING id",
    )
    .bind(Uuid::new_v4())
    .bind(spec.subject.kind.as_str())
    .bind(spec.subject.id)
    .bind(spec.subject.name())
    .bind(spec.target.kind.as_str())
    .bind(spec.target.id)
    .bind(spec.access.as_str())
    .bind(spec.source.as_str())
    .bind(spec.inherited_from)
    .bind(spec.by.actor_id)
    .bind(spec.by.principal_kind.as_str())
    .bind(spec.by.principal_id)
    .bind(&spec.reason)
    .bind(spec.expires_at)
    .bind(hive_db::now())
    .fetch_scalar(db)
    .await
    .map_err(|e| StoreError::db("write grant", e))
}

/// Deletes a grant. Deleting rather than flagging is deliberate: every
/// inherited child goes with it through the foreign key cascade, so "revoking a
/// parent removes every inherited child" is a database property rather than
/// something application code has to remember to do.
pub async fn revoke_grant(db: &Connection, id: Uuid) -> Result<()> {
    let n = query("DELETE FROM grants WHERE id = ?1")
        .bind(id)
        .execute(db)
        .await
        .map_err(|e| StoreError::db("revoke grant", e))?;
    if n == 0 {
        return Err(StoreError::NoRows);
    }
    Ok(())
}

/// What `unshare` actually did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UnshareResult {
    /// Inherited rows revoked in place. Reversible: re-share the parent and
    /// they re-materialize.
    pub tombstoned: i64,
    /// Directly-issued rows removed. Irreversible, and it cascades to
    /// everything that inherited from them.
    pub deleted: i64,
}

/// Removes access on (subject, target).
///
/// Two operations wear one name and they are not equivalent. An INHERITED row
/// is tombstoned, which is what stops the materializer resurrecting a
/// deliberately narrowed child; a DIRECT row is DELETED, because tombstoning
/// one occupies the exact slot a re-share needs.
///
/// `delete_direct` is the caller stating intent. Without it, a subject that has
/// a direct grant returns `WouldDeleteDirectGrant` and NOTHING is changed.
/// Attention is not a safety mechanism; intent is. It takes a transaction so
/// the refusal and the writes cannot interleave with another writer between
/// the check and the act: the Postgres version was one statement, and a
/// `BEGIN IMMEDIATE` is the same guarantee here.
pub async fn unshare(
    tx: &Transaction,
    subj: &Subject,
    target: Owner,
    by: Uuid,
    delete_direct: bool,
) -> Result<UnshareResult> {
    let directs: i64 = query(
        "SELECT count(*) FROM grants g
          WHERE g.subject_kind = ?1 AND g.subject_id = ?2
            AND g.subject_name IS ?3
            AND g.target_kind = ?4 AND g.target_id = ?5
            AND g.source = 'direct' AND g.revoked_at IS NULL",
    )
    .bind(subj.kind.as_str())
    .bind(subj.id)
    .bind(subj.name())
    .bind(target.kind.as_str())
    .bind(target.id)
    .fetch_scalar(tx)
    .await
    .map_err(|e| StoreError::db("unshare", e))?;
    if directs > 0 && !delete_direct {
        return Err(StoreError::WouldDeleteDirectGrant(format!(
            "unshare would delete {directs} directly-issued grant(s), which cannot be undone; \
             say so explicitly or narrow only the inherited ones"
        )));
    }
    let tombstoned = query(
        "UPDATE grants
            SET revoked_at = ?6, revoked_by = ?7
          WHERE subject_kind = ?1 AND subject_id = ?2
            AND subject_name IS ?3
            AND target_kind = ?4 AND target_id = ?5
            AND source = 'inherited' AND revoked_at IS NULL",
    )
    .bind(subj.kind.as_str())
    .bind(subj.id)
    .bind(subj.name())
    .bind(target.kind.as_str())
    .bind(target.id)
    .bind(hive_db::now())
    .bind(by)
    .execute(tx)
    .await
    .map_err(|e| StoreError::db("unshare: narrow inherited", e))?;
    let deleted = if delete_direct {
        query(
            "DELETE FROM grants
              WHERE subject_kind = ?1 AND subject_id = ?2
                AND subject_name IS ?3
                AND target_kind = ?4 AND target_id = ?5
                AND source = 'direct' AND revoked_at IS NULL",
        )
        .bind(subj.kind.as_str())
        .bind(subj.id)
        .bind(subj.name())
        .bind(target.kind.as_str())
        .bind(target.id)
        .execute(tx)
        .await
        .map_err(|e| StoreError::db("unshare: delete direct", e))?
    } else {
        0
    };
    Ok(UnshareResult {
        tombstoned: tombstoned as i64,
        deleted: deleted as i64,
    })
}

/// Copies every live, non-override grant from parent to child as real rows
/// carrying `inherited_from` (D18.3: materialized, never computed ... a computed
/// walk means revocation has to reason about paths, and that is where the
/// holes live).
///
/// Two behaviours fall out of the unique index rather than needing code: a
/// deliberately narrowed child stays narrowed, because its tombstone row still
/// occupies the key; and a parent that was revoked and re-granted gets a new
/// grant id, so its children re-materialize under the new parent.
pub async fn materialize_inherited(
    db: &Connection,
    parent: &Subject,
    child: &Subject,
    by: &Credential,
) -> Result<u64> {
    // One id per row the SELECT produces, minted in SQL: the column default
    // does not apply to INSERT ... SELECT with the column named, and a bound
    // value would give every row the same id.
    query(&format!(
        "INSERT INTO grants (
             id, subject_kind, subject_id, subject_name,
             target_kind, target_id, access, source, inherited_from,
             granted_by_actor, granted_by_principal_kind, granted_by_principal_id,
             reason, expires_at, created_at)
         SELECT {uuid}, ?4, ?5, ?6,
                p.target_kind, p.target_id, p.access, 'inherited', p.id,
                ?7, ?8, ?9,
                'inherited from ' || p.subject_kind || ' ' || p.subject_id,
                p.expires_at, ?10
           FROM grants p
          WHERE p.subject_kind = ?1 AND p.subject_id = ?2
            AND p.subject_name IS ?3
            AND p.source <> 'override'
            AND p.revoked_at IS NULL
            AND (p.expires_at IS NULL OR p.expires_at > ?10)
         ON CONFLICT DO NOTHING",
        uuid = hive_db::UUID_SQL
    ))
    .bind(parent.kind.as_str())
    .bind(parent.id)
    .bind(parent.name())
    .bind(child.kind.as_str())
    .bind(child.id)
    .bind(child.name())
    .bind(by.actor_id)
    .bind(by.principal_kind.as_str())
    .bind(by.principal_id)
    .bind(hive_db::now())
    .execute(db)
    .await
    .map_err(|e| StoreError::db("materialize inherited grants", e))
}

/// Writes a time-boxed override grant (D18.2). It is a grant produced by
/// policy, evaluated in the same predicate as every other grant, so there is no
/// second code path answering "may this actor do this". The database refuses it
/// unless the subject is org-owned and the actor is a human admin of that org.
///
/// Dead rows for the same (subject, admin) are reaped first. Nothing else reaps
/// expired grants, and this is the one path that has to work at 3am under
/// stress, so it cleans up after itself rather than depending on a sweeper
/// somebody has not written yet.
pub async fn enter_break_glass(
    conn: &Connection,
    subj: &Subject,
    admin: &Credential,
    window: Duration,
    reason: &str,
) -> Result<Uuid> {
    if window.is_zero() {
        return Err(StoreError::Other(
            "break-glass needs a positive window".into(),
        ));
    }
    if reason.is_empty() {
        return Err(StoreError::Other("break-glass needs a reason".into()));
    }
    query(
        "DELETE FROM grants
          WHERE source = 'override'
            AND subject_kind = ?1 AND subject_id = ?2
            AND subject_name IS ?3
            AND target_kind = 'user' AND target_id = ?4
            AND (revoked_at IS NOT NULL OR expires_at <= ?5)",
    )
    .bind(subj.kind.as_str())
    .bind(subj.id)
    .bind(subj.name())
    .bind(admin.actor_id)
    .bind(hive_db::now())
    .execute(conn)
    .await
    .map_err(|e| StoreError::db("reap expired break-glass", e))?;

    let expires =
        Utc::now() + chrono::Duration::from_std(window).unwrap_or(chrono::Duration::hours(1));
    write_grant(
        conn,
        &GrantSpec {
            subject: subj.clone(),
            target: Owner::new(PrincipalKind::User, admin.actor_id),
            access: Access::Read,
            source: GrantSource::Override,
            inherited_from: None,
            by: *admin,
            reason: reason.to_string(),
            expires_at: Some(expires),
        },
    )
    .await
}
