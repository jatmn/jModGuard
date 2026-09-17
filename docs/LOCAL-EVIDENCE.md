# Local evidence transfers

The opt-in Unix `evidence_transfer::LocalEvidenceStore` implements real file capture,
verification, recovery and cleanup against the [evidence journal](CASE-EVIDENCE.md).
It is a library backend for a dedicated local filesystem. Bot and API now serve available
objects through [tenant runtime](TENANT-RUNTIME.md). The bot captures proving Discord
attachments for selected image-spam enforcement and streams them into this backend
through that runtime after a ban case is recorded; the API does not perform that Discord
re-fetch. There is no background capture worker. Core is at migration 0012; the current
guild schema is 8.

## Resource contract

Trusted installation code supplies an absolute root, a stable nonsecret storage ID and
an explicit positive buffer size. `initialize` creates a private root or accepts an empty
private directory; it refuses a nonempty unmarked resource. `open` requires the exact
persisted identity and existing stable lock file. Neither method adopts arbitrary files.
The root's parent must already exist and be trusted. Keep this evidence resource separate
from the backups-only resource. Identity binding is not a backup resource classifier:
the operator must choose the correct dedicated directory.

Directories have no group/other permissions; files are private regular files with one
hard link. Root, owned directory and artifact checks reject symlinks, foreign identities,
nonregular files and extra hard links. This assumes trusted ancestors and no hostile
process running as the same OS user. It is not a sandbox against administrators changing
paths or replacing the database. Restored/cloned identities alone do not fence writers.
Do not put this backend on a network filesystem or use uncoordinated writers.

Operations reload the exact captured guild/enrollment/case/evidence identity from SQL;
callers cannot supply an arbitrary object key. The journal's derived key names only three
artifacts: `.data`, `.complete` and `.complete-tmp`. No URL, credential or source filename
is stored. The supplied blocking `Read` is trusted input: attachment ownership, permission,
source selection and bounded Discord CDN timeouts belong to the bot Hasher integration.

## Capture and recovery

`capture` reserves the journal identity before consuming the source. Only a newly created
reservation may upload. An existing reservation is an error requiring explicit
`reconcile`; retrying it never reads the source or overwrites the object.

Streaming uses one caller-sized buffer and incremental SHA-256. It rejects short streams
and reads at most one byte beyond the reserved size before rejecting oversized streams.
The completed data file is synced, reopened and hashed again. A size/checksum receipt is
written to a temporary file, synced, renamed to `.complete`, and its directory synced
before SQL can become available. No whole attachment is buffered in memory. SHA-256 uses
the existing `ring` package already required by rustls; it is now also an explicit direct
dependency, with no additional package versions in the lockfile.

`reconcile` never retries the source upload. While holding the resource lock it checks the
completion receipt, hashes the file, and syncs the verified files/directory before settlement.
This also covers a prior process dying between receipt rename and directory sync. A valid
completed capture can finish SQL settlement
after an interrupted/lost result. Incomplete data or a missing receipt becomes cleanup-pending;
corrupt completed data also becomes cleanup-pending and returns an error. Cancelled capture
is discarded. Unsafe paths or database failures remain unresolved instead of acknowledging
cleanup. Non-capturing rows return their journal state; this is not an integrity scan or
an access grant. Serving will need fresh membership/parent and backend authorization checks.

A clone shares one nonqueueing admission permit. Each operation also takes a nonblocking
[OS file lock](https://doc.rust-lang.org/std/fs/struct.File.html#method.try_lock) on the stable
resource lock file, excluding other cooperating handles/processes through I/O and SQL
settlement. Busy operations fail for a future scheduler to retry. Cancelling the async
waiter does not release the blocking task's permit or lock; the task completes its current
operation. A blocking reader must implement its own I/O deadlines. Process termination
releases the OS lock, after which recovery inspects the durable artifacts.

This local lock protocol does not prove that a timed-out remote PUT has stopped. The separate [Bunny adapter](BUNNY-EVIDENCE.md)
uses a durable single-use attempt and acceptance receipt;
an HTTP timeout or object 404 cannot substitute for exclusive local ownership.

## Removal

Case removal or closed-membership cancellation hides evidence through the journal. For
available objects in a closed membership, run bounded `cancel_closed_evidence` preparation
first. `cleanup` can settle a cancelled capturing row while holding the exclusive local
lock. It refuses active capturing/available rows and wrong case/resource identities.

Cleanup removes only the three owned artifacts, syncs removal, prunes empty key directories,
and verifies absence before acknowledging SQL deletion. Other objects, resource identity
and lock remain. File errors keep cleanup pending. If SQL acknowledgment fails after file
removal, retry verifies absence and completes it. An already-erased retry is a no-op.
Parent purge remains blocked until the journal has acknowledged evidence erasure.
This does not erase backup copies or complete the entire guild lifecycle.

## Validation and remaining integration

Local tests exercise real bytes/hashes, stream bounds/failures, interrupted settlement and
reopen, incomplete/corrupt recovery, cancellation during a blocked read, actual separate-
process exclusion, identity/path guards, removal retries, preservation of unrelated objects
and closed-membership purge. Disposable libSQL also exercises real local files alongside
lost reservation and deletion-acknowledgment responses.

Live Bunny validation, durable worker scheduling, and portable backup-copy cleanup
remain separate work. Authorized serving, runtime routing and Cases UI for this backend
are implemented; see [tenant runtime](TENANT-RUNTIME.md). Local filesystem tests establish neither
provider behavior nor billed CPU/memory use.
