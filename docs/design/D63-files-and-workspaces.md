# D63: files are a fourth storage primitive over blobs: paths and directories per context, every file a blob by reference, versions kept; a workspace is a named file tree a person, an org, an install or an agent run owns, mounted into the D53 containers and the harness runs; one `files` surface on the storage interface, drivers behind it like every other primitive

**Decided** 2026-10-14 by Nate: "i would love the workspace and a standard
file system storage driver for apps and users/orgs as well abstracting
on-top of the blob storage." Shape by Propolis. Builds on D40 and D11
(blobs on the mounted path behind a driver seam; the catalogue where
references, ownership and trust live), invariant 3 (ownership, permission
and trust are properties of a reference, never of bytes), invariant 8 (no
blob without a reference), D52 (the storage interface, its drivers and the
conformance suite), D53 (apps as processes in containers with a read-only
root and the daemon's socket), D35 §6 (the harness container's volumes),
D26 (checkout storage), D45 (offline replicas), D51 (sealed contexts) and
D60 (the operation log). **Amends D52 §2**: the interface has four
primitives, not three.

## What exists, and what the word "workspace" already means here

- **Blobs** are content-addressed bytes on the blob root with a catalogue
  of references (D40). A reference carries owner, permission and trust;
  the bytes carry nothing. Two documents may reference one blob. Nothing
  gives a blob a *name*, and nothing groups blobs into a tree.
- **A harness run has a working directory**: the container gets a volume
  with the checkout or the task's files, written by the run, and read back
  by the host when the run ends (D35 §6, D26's checkout storage). It is a
  directory on a node, not a thing the platform names, owns or keeps.
- **An app has no files.** D53 §2 makes the storage interface the only
  way an app keeps data, with a read-only root and tmpfs scratch, and the
  interface has blobs, tables and documents. An app that wants a tree of
  named files (a media library's artwork, a project's sources, a person's
  uploads) has to model it as documents pointing at blobs, each app its
  own way.
- **A person's files** live in their devices' replicas (D45) and in blobs
  the client uploads; there is no "my files" that an app, an agent and the
  person all see as the same tree.

## The decision

### 1. Files: a tree of names over blobs

- **A file** is a path within a **workspace** (§2), pointing at a blob
  reference, with its own metadata (size, media type, created and updated
  times, the author actor, trust, a version number) and the platform's
  columns every row carries (owner, HLC, key version). **A directory** is
  a path with children and no blob. Paths are the usual slash-separated
  Unicode names, normalised once at the write (no `.`, `..`, empty
  segments or control characters; a refused path says why).
- **Every file is a reference.** Writing a file puts bytes through the
  blob catalogue (`add_ref`, D40) and records the reference on the file
  row; two files may point at one blob; deleting a file releases its
  reference and the catalogue's rule decides whether the bytes go
  (invariant 8). Ownership, permission and trust are the file row's,
  which is the reference's (invariant 3): a hash is never a file.
- **Versions are kept.** Overwriting a file writes a new blob reference
  and keeps the previous as a version row with its own metadata; a
  workspace's retention says how many or how long; `files.history` lists
  them and `files.restore` makes one current again. This is the same shape
  D60's operation log gives every write, and the file version rows are
  what the log's per-field delta points at for a `files.write`.
- **The verbs**, on the D52 interface with the same `{trust, data}`
  envelope and `Guard` before any driver: `files.list {workspace, path}`,
  `files.stat`, `files.read` (a stream, D58, with ranges), `files.write`
  (a stream in), `files.mkdir`, `files.move`, `files.copy` (a new
  reference to the same blob, no bytes copied), `files.delete`,
  `files.history`, `files.restore`, `files.watch` (events on the bus for a
  subtree). Trust on a read is the file's; a write inherits the writer's
  invocation taint (invariant 12).
- **Grants** are by workspace and path prefix: `subject_kind = 'files'`,
  the workspace as the subject, an optional path prefix as the subject
  name, verbs `read`, `write`, `admin`. Absence is deny. A share of one
  file to another person is a grant on its path (D13's row-sharing shape,
  on a path).
- **Sealed contexts (D51)**: file bytes are blobs and are sealed as blobs
  are; names and metadata are content too and are sealed with the row,
  which means `files.list` in a sealed context works on devices and on
  the server only under an unlock or in served mode, as D52 §1 says for
  any typed column. A workspace may be declared `names-plain` by its owner
  to keep listing server-side while bytes stay sealed; the storage page
  shows the choice.

### 2. Workspaces: a named tree someone owns

- **A workspace** is a root the files of §1 hang under, owned by exactly
  one context and named within it: `person:<id>/home`, `org:<id>/shared`,
  `install:<id>/data`, `run:<id>/work`. Every person and org gets a default
  workspace at sign-up (D52 §1's provisioning); every install gets one at
  install; every agent run gets one at start.
- **An install's workspace** is where an app keeps its named files (the
  artwork, the uploads, the generated reports) through `hive.files` in
  `@hive/app` and `hive_app`, the way `hive.tables` is where it keeps
  rows. The read-only root of D53 stays; the workspace is reached through
  the interface or through a **mount** (§3), never through the container's
  own filesystem.
- **A run's workspace** is what the harness gives the run as its working
  directory: created from a declared source (an install's workspace, a
  person's folder, a git checkout per D26, or empty), mounted into the
  container (§3), and kept when the run ends under the run's TTL (D54's
  purposes apply: a `run` workspace expires like a `run` branch), so what
  an agent produced is a tree a person can open, diff and keep, not a
  volume on a node.
- **A person's workspace** is their files across devices: the client (D45)
  syncs a workspace's tree and metadata as it syncs tables and documents,
  and fetches bytes by reference under the per-device blob limit D45 §2
  already sets.
- **Across contexts**: a workspace is in one context; sharing a subtree to
  another person or an org is a grant on a path prefix, never a move;
  moving a file into an org's workspace is a copy of the reference under
  the org's ownership plus a release of the person's, with D46 §1's
  warning when the file was private.

### 3. Mounts: the tree as a filesystem inside a container

- A D53 container or a harness run may declare `mounts: [{workspace,
  path, mode}]`; the daemon mounts the workspace at the path inside the
  container, read-only or read-write per the install's or run's grants.
  The mount is served by the daemon (a FUSE filesystem on the node backed
  by `files.*` under the container's credential), so every read and write
  through it is a `Guard` decision and an operation (D60), and the
  container never sees the blob root, the catalogue or another workspace.
  What a process writes through the mount is a `files.write` with the
  process's credential; nothing bypasses the interface by being a
  filesystem.
- Mounts are the one place this record touches the sandbox: a mount is
  declared in the manifest, shown at install, and refused when the grant
  is absent (D53 §1's shape). The FUSE server is part of `hive-harness`
  and runs where the containers run; the single-box install has it
  because it has Podman (D53 §3).
- Performance: a mount is for a working set, not a media library; `files.
  read` as a D58 stream is the path for large sequential reads, and the
  record names the measured numbers as open.

### 4. Drivers

As D52 §3: the default `FileDriver` keeps file rows in the org's database
(tables, under the storage interface's own schema) and bytes in the blob
driver already bound for the context. An added driver (an org's own object
store, a NAS export) binds per `(org, primitive, generation)` after the
conformance suite, stores bytes and metadata as it likes, and never
decides access, sees plaintext in a sealed context, or changes the shape
of a path. A `files` driver over a plain directory on a mounted path is
the first added one, because the household's media sits on the NAS and
nectar (D56) wants to name it without copying it: such a driver imports
existing files as references with the mount's own metadata, and the
record says that an imported tree is `untrusted` until the owner says
otherwise (invariant 9's shape for content from outside).

## Acceptance scenarios, checked in before the code

1. **A file is a reference.** Writing the same bytes to two paths yields
   one blob and two file rows; deleting one path leaves the bytes while the
   other references them; deleting both releases them (the catalogue's
   count, asserted).
2. **Grants by path prefix.** A grant on `org/shared/projects/` reads
   `projects/a/notes.md` and is refused on `projects-old/`, with the grant
   message; a share of one file reaches that file and nothing beside it.
3. **Versions and restore.** Three writes to one path leave one current
   and two versions with their authors; `restore` of the first makes it
   current and records a fourth version; retention trims the oldest when
   the workspace's limit is passed.
4. **A path that is not a path is refused** with the message: `..`, an
   empty segment, a control character, a name past the bound.
5. **A mount is the interface.** Inside a test container with the install's
   workspace mounted read-write, a process writes a file; the row exists
   with the install's actor and principal pinned by the daemon; a write to
   a path the grant lacks fails inside the container with EACCES and the
   daemon's event names the refusal; the container can see no path outside
   the mount.
6. **A run's workspace outlives the run.** A harness run writes into its
   workspace; after the run ends the tree is listable under `run:<id>/work`
   until its TTL; a person with the grant reads it.
7. **Streams and ranges.** `files.read` on a 100 MB file with a range
   returns `206` and the bytes (D58 scenario 2 through `files`).
8. **Sealed names.** In a sealed context, `files.list` on the server
   without an unlock is refused with the message naming served mode; the
   same workspace declared `names-plain` lists on the server and its bytes
   stay ciphertext to the driver (D52 scenario 5 through `files`).
9. **The NAS driver imports as untrusted.** A directory driver over a test
   tree imports its files as references marked `untrusted` with the mount's
   metadata, and the owner's mark raises them under audit (invariant 12's
   sanitise path).

## Order

1. **The file and workspace tables and the verbs** over the default
   driver, with default workspaces at sign-up and install (D52's
   provisioning). Scenarios 1 to 4.
2. **`files.read` and `files.write` as streams** (after D58). Scenario 7.
3. **Run workspaces** in the harness, replacing the ad hoc volume (D35
   §6). Scenario 6.
4. **Mounts** through the FUSE server in `hive-harness`. Scenario 5.
5. **Sync of a person's workspace** in the client (D45's slices). 
6. **Sealed names and `names-plain`** with D51. Scenario 8.
7. **The directory driver** for the NAS, with nectar's library as the
   first import (D56). Scenario 9.

After D43 slice 3's remaining crates (the blob catalogue on Postgres is
among them) and the D53 runtime.

## What lost

- *Files as documents pointing at blobs, per app.* Every app its own tree
  shape and no mount, no sharing by path, no person's files.
- *A workspace as a volume on a node.* What a run produced would stay
  where it ran, unnamed and unowned.
- *A hash as a file.* Invariant 3; a file is a reference with a name.
- *Mounts that bypass the interface.* A filesystem that reaches the blob
  root directly is a second enforcement point with a mount table for a
  grant list.
- *Importing a NAS tree as trusted.* It came from outside the platform's
  writers; invariant 9.
- *A fifth primitive for "folders".* A directory is a path with children;
  the tree is the primitive.

## Open

- Mount throughput and latency through FUSE on the cluster's nodes,
  measured before the mount is relied on for anything but a working set.
- Retention defaults per workspace kind, and quotas (D52's open quota item
  grows a fourth primitive).
- Whether a run's workspace may be branched (D54) with the org's data;
  assumed yes once both exist, not designed here.
- Conflict handling for a person's workspace across devices: D45 §3's
  per-field rule applies to metadata; concurrent writes to one file's
  bytes keep both versions, which §1 already does.
- The directory driver's change detection for a NAS tree that others
  write to (a scan, inotify where the mount gives it); nectar's scanner is
  the prior art.
