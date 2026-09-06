# D33: cross-app collection access is decided on the asking install, not only on the principal

**Decided** 2026-09-06 by Propolis, while building #86. It corrects a mechanism
detail of [D32](D32-online-first-pluggable.md) section 3, which is Nate's
decision; the goal there is unchanged and only the seam moves. Recorded before
the code, because the finding changes what #86 has to build.

## What was found

D32 section 3 says an app's manifest declares the collections it **uses**, the
registry derives the collection grants the install needs, and activating the
install writes them on the core install "through the existing grants seam".
Written that way, the grants decide nothing.

`subject_owner()` resolves a `collection` subject to the **install's** owner.
`access_decision()`'s first branch is "the principal owns the row", and it is
reached before any grant is considered. The predicate is never told which
install is asking. So for any two installs owned by the same principal, a call
naming the other's collection returns `owner`, and the derived grant is never
consulted.

Reproduced first, against the migrations, in
`crates/hive-store/tests/invariants.rs`:
`owning_two_apps_is_not_a_reason_for_one_to_read_the_other` fails with
`Some("owner")` where deny is required, beside a control
(`an_owner_reaches_their_own_apps_collection`) that passes ... so the pair can
tell a denial from a predicate that cannot see collections at all.

This is invariant 14. The key omits a dimension the decision depends on, and
the omitted dimension ("which app is asking") is the entire content of the
`uses` declaration. It is also the **bad** half of the collision question
CLAUDE.md poses: it does not fail closed. It resolves to `owner`, the read
succeeds, and the audit records it honestly as the principal's own access,
because it *is* the principal's own access. Nothing looks wrong afterwards.

The threat model is not hypothetical. #27 is the builder loop and #19 starts an
AI-built `local` app with no capabilities at all. Under the seam as D32
describes it, such an app, installed by the person it was built for, reads that
person's contacts, notes and decisions without declaring anything.

## The decision

**Cross-install collection access requires two conditions, both evaluated
inside the one predicate:**

1. the **principal** is authorized for the subject, exactly as today (owner, a
   grant, an org grant, or an audited override); and
2. the **asking install** holds a live grant for that collection, when the
   asking install is not the install that owns the collection.

`access_decision()` gains an acting-install argument. When the subject is a
`collection` and the acting install differs from the subject's install, the
`owner` branch stops being sufficient on its own: it still has to be satisfied,
and it no longer ends the decision.

Grants gain `target_kind = 'install'`. That is what "the existing grants seam"
becomes rather than a second table: revocation, expiry, inheritance,
provenance and the override audit are all properties of a grant row already,
and a parallel `install_uses` table would have to grow every one of them again
and would be a second enforcement point the day it disagreed.

**Both conditions, not either.** The principal check keeps invariant 2 intact:
widening to the app must never widen past the person. The install check is the
new dimension. An app cannot reach what its owner cannot, and an owner's apps
cannot reach each other unattended.

## Why not the alternatives

- *Check the `uses` declaration in the storage layer and leave the predicate
  alone.* This is the tempting one, because it needs no migration. It puts an
  access decision outside the single enforcement point, and the check would sit
  in the one place the caller's own manifest is in scope ... which is invariant
  11's failure mode in its purest form, a check consulting a fact the asking
  side supplied.
- *A separate `install_uses` table consulted by the predicate.* Two tables
  answering one question, and the whole grant machinery duplicated. It also
  hides the relationship from `unshare` and from the provenance path, so a
  person could not see why an app can read their contacts.
- *Give each app its own principal.* It would work and it destroys invariant 2:
  ownership would move from the person to the app, and "Nate's contacts" would
  stop being a true sentence about the row.
- *Treat the core install as public within an owner.* This is what the code
  does today by accident. It is a defensible product decision and an
  indefensible default, and it cannot be narrowed later without breaking every
  app written against it.

## What this costs

A migration that changes `access_decision`'s signature, which every caller of
the predicate passes through `Guard`. `Guard` is the only caller (invariant 1),
so the blast radius is one crate, but the acting install has to travel from the
invocation to the guard on every storage path, and a `None` there must mean
"not a guest call" rather than "no restriction" ... the default has to be the
strict one, or this decision is a comment.

## Left open

- Whether a `write` grant to another app's collection is offered at all in
  #86, or only `read` until there is a case for it. D32 says `read|write`;
  starting read-only is reversible and the opposite is not.
- Whether the core install per owner is created lazily at first use or eagerly
  with the principal. D32 says with the principal; nothing here disagrees, it
  is simply not settled by this record.
- The `uses` manifest syntax and the registry's derivation of it, which is the
  rest of #86.
