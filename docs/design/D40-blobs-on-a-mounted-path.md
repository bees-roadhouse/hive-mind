# D40: blobs live on a mounted path; replication is the mount's job; the S3 driver goes

**Status:** decided 2026-10-02 by Nate ("can we just support local
filesystem for blob storage and replication? maybe take advantage of
juicefs?" ... "obviously TiKV"), recorded here.

## What was asked

Drop the second blob backend. Store bytes on a local path, and get
replication and multi-node access from the filesystem underneath that path,
JuiceFS with TiKV as its metadata engine being the one he intends to run.

## The decision

### 1. One driver: disk, on whatever is mounted at `--blob-root`

The disk driver is already a filesystem driver: `<hh>/<sha256>` under a
root, content-addressed, with ownership and trust in `blob_refs` rather
than on disk (invariant 3, D17.1). The daemon does not know and must not
care whether the root is a local disk, a ZFS dataset, or a JuiceFS mount.
Replication, snapshots and multi-node reads are properties of the mount,
and putting them below the seam is what keeps the seam honest: nothing
above it changes when the mount does.

The driver seam from D11 stays, with one implementation. It costs one trait
and it is the thing that let the S3 driver be added and removed without
touching the catalogue; a third backend, if one is ever wanted, is a
config-time choice again.

### 2. The S3 driver, Garage, and their tier are removed

`S3Driver`, the three `aws-*` crates, `scripts/garage-*`, the Garage compose
file and config, the `blobstore` CI job and the `HIVE_SANDBOX_TEST_S3_*`
variables go. That is one of the three test tiers that needed a backend
gone, which is the direction D38 set with Postgres.

What is lost with it, said plainly: **redirect delivery.** The S3 driver
could hand a client a signed URL so large bytes never crossed the daemon;
the disk driver proxies every read. For a self-hosted household platform
the daemon is on the path anyway. The rule the S3 driver existed to test,
that a scriptable type can never be redirected because S3 cannot set
`nosniff`, stays in the code (`scriptable_mime`, `plan_delivery`) because
the proxy path sets the same headers and the rule is the reason it does.

### 3. The store files stay on local disk

JuiceFS is a POSIX filesystem over an object store, and SQLite in WAL mode
needs shared memory and byte-range locks on one host. The control plane
and the owner files (D38, D39) are not on the mount; `--data-dir` is local
disk, and replicating it is D38's phase 3 with its own tools (Litestream,
or libSQL's replication once the fork's close bug is fixed). Blobs on the
mount, the store beside it on local disk, is the split.

### 4. What the mount needs from us

Nothing the disk driver does not already do: the uploads spool is under
the same root, a seal is a rename, and a delete is an unlink. JuiceFS
promises POSIX rename atomicity on one mount, which is all the driver
relies on. Two daemons sharing one mount is not a supported shape today
and is not made one here: the catalogue is in the control plane, which is
per daemon.

## What lost

- *Keep both drivers.* Two backends is two tiers to keep green for a
  choice nobody is going to make; the seam keeps the option without the
  cost.
- *Put the store on the mount too.* §3.
- *A JuiceFS-specific driver.* There is nothing JuiceFS-specific to do;
  it is a path.

## Open

- Nothing in the daemon. The mount itself (JuiceFS on TiKV) is brh-infra's
  to bring up.
