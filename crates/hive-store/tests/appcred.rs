//! The install credential (D53 §1, #163 slice 2): an actor per install, a
//! credential minted and revoked with the container, the install dimension
//! carried by the actor. Runs on both engines through `TestDb`.

mod common;

use common::{World, cred, org, user};
use hive_identity::PrincipalKind;
use hive_store::{
    INSTALL_CREDENTIAL_LABEL, StoreError, ensure_install_actor, install_of_actor, install_persona,
    issue_install_credential, resolve_credential, revoke_all_install_credentials,
    revoke_install_credentials,
};

/// An install gets one actor, created by the installing person, acting for
/// the install's owner; asking twice gives the same one.
#[tokio::test]
async fn an_install_gets_one_actor_acting_for_its_owner() {
    let w = World::new("appcred_actor").await;
    let alice = w.human("alice").await;
    let install = w.install("notes", user(alice), alice).await;
    let by = cred(alice, PrincipalKind::User, alice);
    let c = w.conn().await;

    let actor = ensure_install_actor(&c, install, &by).await.expect("actor");
    let again = ensure_install_actor(&c, install, &by).await.expect("again");
    assert_eq!(actor, again, "a second call made a second actor");

    let row = hive_db::query(
        "SELECT kind, persona, principal_kind, principal_id, created_by_actor FROM actors WHERE id = ?1",
    )
    .bind(actor)
    .fetch_one(&c)
    .await
    .unwrap();
    assert_eq!(row.get::<String>("kind"), "ai");
    assert_eq!(row.get::<String>("persona"), install_persona("notes"));
    assert_eq!(row.get::<String>("principal_kind"), "user");
    assert_eq!(row.get::<uuid::Uuid>("principal_id"), alice);
    assert_eq!(row.get::<uuid::Uuid>("created_by_actor"), alice);
    assert_eq!(
        install_of_actor(&c, actor).await.unwrap(),
        Some(install),
        "the actor does not lead back to its install"
    );
    assert_eq!(
        install_of_actor(&c, alice).await.unwrap(),
        None,
        "a person is not an install"
    );
}

/// D19.2 holds here as it holds everywhere: an AI may not create the actor,
/// and a person may not create one acting for someone else. The refusal is
/// the database's, with its message.
#[tokio::test]
async fn the_database_decides_who_may_create_the_actor() {
    let w = World::new("appcred_creator").await;
    let alice = w.human("alice").await;
    let bob = w.human("bob").await;
    let assistant = w.ai("alice-ai", "assistant", user(alice), alice).await;
    let install = w.install("notes", user(alice), alice).await;
    let c = w.conn().await;

    let err = ensure_install_actor(&c, install, &cred(assistant, PrincipalKind::User, alice))
        .await
        .expect_err("an AI created the install's actor");
    assert!(
        format!("{err}").contains("D19.2"),
        "refused for another reason: {err}"
    );
    let err = ensure_install_actor(&c, install, &cred(bob, PrincipalKind::User, bob))
        .await
        .expect_err("bob created an actor acting for alice");
    assert!(
        format!("{err}").contains("D19.2"),
        "refused for another reason: {err}"
    );
    // Neither refusal left a half-made row behind.
    assert_eq!(
        w.count("SELECT count(*) FROM actors WHERE kind = 'ai' AND persona = 'app:notes'")
            .await,
        0
    );
}

/// The token a container starts with resolves to the install's actor acting
/// for the owner, is keyed on the install, and dies with the container.
#[tokio::test]
async fn the_credential_is_the_installs_and_dies_with_the_container() {
    let w = World::new("appcred_lifecycle").await;
    let alice = w.human("alice").await;
    let install = w.install("notes", user(alice), alice).await;
    let by = cred(alice, PrincipalKind::User, alice);
    let c = w.conn().await;
    let actor = ensure_install_actor(&c, install, &by).await.unwrap();

    let (token, id) = issue_install_credential(&c, install).await.expect("issue");
    let resolved = resolve_credential(&c, &token).await.expect("resolve");
    assert_eq!(
        resolved.actor_id, actor,
        "the credential is not the install actor's"
    );
    assert_eq!(
        (resolved.principal_kind, resolved.principal_id),
        (PrincipalKind::User, alice),
        "the credential does not act for the owner"
    );
    let row = hive_db::query("SELECT label, install_id, expires_at FROM credentials WHERE id = ?1")
        .bind(id)
        .fetch_one(&c)
        .await
        .unwrap();
    assert_eq!(row.get::<String>("label"), INSTALL_CREDENTIAL_LABEL);
    assert_eq!(row.get::<Option<uuid::Uuid>>("install_id"), Some(install));
    assert!(
        row.get::<Option<chrono::DateTime<chrono::Utc>>>("expires_at")
            .is_none()
    );

    // Stop: revoked by the install key, and the next call with it is
    // NoCredential, which the edge renders as the one 401.
    let n = revoke_install_credentials(&c, install).await.unwrap();
    assert_eq!(n, 1);
    assert!(matches!(
        resolve_credential(&c, &token).await,
        Err(StoreError::NoCredential)
    ));
    // A second stop revokes nothing: there is nothing live.
    assert_eq!(revoke_install_credentials(&c, install).await.unwrap(), 0);
}

/// Two installs of one app are two actors and two credentials, and each
/// credential leads back to its own install (invariant 14): the app alone is
/// not the key.
#[tokio::test]
async fn two_installs_of_one_app_are_two_actors() {
    let w = World::new("appcred_two_installs").await;
    let alice = w.human("alice").await;
    let bob = w.human("bob").await;
    let a = w.install("notes", user(alice), alice).await;
    let b = w.install("notes", user(bob), bob).await;
    let c = w.conn().await;
    let actor_a = ensure_install_actor(&c, a, &cred(alice, PrincipalKind::User, alice))
        .await
        .unwrap();
    let actor_b = ensure_install_actor(&c, b, &cred(bob, PrincipalKind::User, bob))
        .await
        .unwrap();
    assert_ne!(actor_a, actor_b);
    let (ta, _) = issue_install_credential(&c, a).await.unwrap();
    let (tb, _) = issue_install_credential(&c, b).await.unwrap();
    let ra = resolve_credential(&c, &ta).await.unwrap();
    let rb = resolve_credential(&c, &tb).await.unwrap();
    assert_eq!(install_of_actor(&c, ra.actor_id).await.unwrap(), Some(a));
    assert_eq!(install_of_actor(&c, rb.actor_id).await.unwrap(), Some(b));
    assert_eq!(rb.principal_id, bob, "bob's install acts for bob");
    // Stopping a leaves b running.
    revoke_install_credentials(&c, a).await.unwrap();
    assert!(resolve_credential(&c, &ta).await.is_err());
    assert!(resolve_credential(&c, &tb).await.is_ok());
}

/// A daemon that comes back after a crash revokes every install credential,
/// because every container it supervised died with it; a person's token is
/// not an install's and survives.
#[tokio::test]
async fn a_restart_revokes_install_credentials_and_nothing_else() {
    let w = World::new("appcred_restart").await;
    let alice = w.human("alice").await;
    let install = w.install("notes", user(alice), alice).await;
    let c = w.conn().await;
    ensure_install_actor(&c, install, &cred(alice, PrincipalKind::User, alice))
        .await
        .unwrap();
    let (app_token, _) = issue_install_credential(&c, install).await.unwrap();
    let (person_token, _) = hive_store::issue_credential(
        &c,
        alice,
        user(alice),
        &cred(alice, PrincipalKind::User, alice),
        "laptop",
        None,
    )
    .await
    .unwrap();

    let n = revoke_all_install_credentials(&c).await.unwrap();
    assert_eq!(n, 1, "revoked {n}, expected the one install credential");
    assert!(resolve_credential(&c, &app_token).await.is_err());
    assert!(
        resolve_credential(&c, &person_token).await.is_ok(),
        "a person's token was revoked"
    );
}

/// No actor, no credential: a guest install (the frozen tier) never gets one,
/// and the refusal names the reason rather than failing inside the insert.
#[tokio::test]
async fn an_install_without_an_actor_gets_no_credential() {
    let w = World::new("appcred_no_actor").await;
    let alice = w.human("alice").await;
    let install = w.install("hello", user(alice), alice).await;
    let c = w.conn().await;
    let err = issue_install_credential(&c, install)
        .await
        .expect_err("issued without an actor");
    assert!(format!("{err}").contains("has no actor"), "{err}");
}

/// An org-owned install: the actor acts for the org, the installer is an
/// admin, and the credential's principal is the org.
#[tokio::test]
async fn an_org_install_acts_for_the_org() {
    let w = World::new("appcred_org").await;
    let alice = w.human("alice").await;
    let acme = w.org("acme", alice).await;
    w.member(acme, alice, "admin", alice).await;
    let install = w.install("notes", org(acme), alice).await;
    let c = w.conn().await;
    let actor = ensure_install_actor(&c, install, &cred(alice, PrincipalKind::Org, acme))
        .await
        .unwrap();
    let (token, _) = issue_install_credential(&c, install)
        .await
        .expect("issue for org");
    let r = resolve_credential(&c, &token).await.unwrap();
    assert_eq!(r.actor_id, actor);
    assert_eq!(
        (r.principal_kind, r.principal_id),
        (PrincipalKind::Org, acme)
    );
}
