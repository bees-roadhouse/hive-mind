-- The install credential (D53 §1): a process app reaches the host with a
-- credential minted when its container starts and revoked when it stops,
-- pinned to an AI-shaped actor OF THE INSTALL acting for the install's
-- owner (invariant 2: "an app acting for Nate did this" is distinguishable
-- from "Nate did this" on every request). The actor is the install's and not
-- the app's: two installs of one app are two actors, so the credential a
-- container holds carries the install dimension by construction and the
-- predicate can find which install is asking from the actor alone
-- (invariant 14, the eighth instance was a predicate never told which
-- install was asking).

-- Nullable: a guest install (the frozen WASM tier, D53 §5) has no process
-- and no actor, and every install that exists today is one.
ALTER TABLE installs ADD COLUMN actor_id TEXT REFERENCES actors (id);

-- One actor serves one install. The other direction is the actors row's
-- principal, which is the install's owner.
CREATE UNIQUE INDEX installs_actor_idx ON installs (actor_id) WHERE actor_id IS NOT NULL;

-- Which install a credential was minted for, so revocation at container
-- stop is a key and not a label match. Nullable: a person's credentials
-- are for no install.
ALTER TABLE credentials ADD COLUMN install_id TEXT REFERENCES installs (id) ON DELETE CASCADE;

CREATE INDEX credentials_install_idx ON credentials (install_id) WHERE install_id IS NOT NULL AND revoked_at IS NULL;
