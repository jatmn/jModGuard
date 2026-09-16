# Bunny evidence transfers

`bunny_evidence::BunnyEvidenceStore` is an opt-in Unix library adapter for selected case
attachments. It performs real HTTP transfers and uses the guild journal for durable progress.
Current bot/API/conductor services do not call Bunny HTTP from a background worker.
Tenant runtime can stream a local capture file into this adapter after selected
image-spam enforcement; Discord CDN fetch stays on the Hasher contract. Operator
resolution of uncertain uploads and backup-copy lifecycle are not enabled.

## Installation boundary

Trusted installation code supplies a storage alias, primary regional hostname, zone name,
zone password and explicit buffer/timeout limits. Production construction accepts only the
[Bunny Storage HTTP endpoints](https://bunny.net/docs/storage/http), with HTTPS and a simple
zone-name path segment. Credentials use `AccessKey`; this is the zone password, not the
account API key. There are no arbitrary request URLs, redirects, proxies, automatic request
retries or decompression. The client reuses the existing hyper/rustls connector and ring
hashing, sharing its connection pool among clones. No SDK or new locked package is added.

Use a dedicated **private evidence zone**, separate from the backups-only zone. The installer
must verify that no public delivery is attached and provision `jmodguard-evidence-identity`
with exactly `jmodguard-bunny-evidence-1\n{storage_id}\n` (actual newlines, including the last).
Every operation verifies this small authenticated marker. The adapter never creates/adopts
zones or markers, lists arbitrary directories, or deletes a directory/root. A marker proves
matching configured identity, not absence of a Pull Zone: the storage password does not give
this adapter a control-plane privacy audit. Installer verification and protection of that
binding remain prerequisites to runtime activation. Administrators can copy markers or
restore databases; these checks do not fence such copies or hostile credential holders.

Object keys come from freshly loaded captured guild/enrollment/case/evidence identities in
the journal. No caller-supplied key, raw attachment URL or filename reaches the storage path.
The resolved regional zone binding is persisted with each attempt; a different binding for
the same alias is refused while another attempt in that guild store retains it. Recovery and
cleanup require an exact binding match. Installation-wide alias/credential management remains
trusted lifecycle work across databases. Keys remain stable across future guild-store moves.

## Capture and recovery

The caller supplies a seekable async source and explicit journal admission limits. The adapter
checks existing identity before reading; a raced reservation is also refused before PUT. It
hashes the bounded source, rewinds, reserves ownership, and commits a single-use PUT attempt
before sending bytes. Upload uses raw bytes, exact Content-Length and uppercase SHA-256 in the
[Checksum header](https://bunny.net/docs/storage/http#with-checksum). It hashes the source again
during transfer; short, oversized or changed sources cannot become available.

Only a completed source transfer plus HTTP201 records provider acceptance. After that durable
receipt, an authenticated [GET](https://bunny.net/docs/api-reference/storage/manage-files/download-file)
verifies the actual byte count and hash before the ownership journal becomes available.
Read failure or corrupt data after acceptance remains recoverable: `reconcile` repeats
verification without another PUT. A cancelled accepted capture settles as discarded.

A timeout, disconnect, non201 status, or crash before persisting acceptance leaves the attempt
unresolved. Even a 400/401 response is handled conservatively. A matching GET, early404, elapsed
waiting period or restarted process cannot upgrade that state or authorize deletion. SQL guards
also refuse generic capture settlement for an unaccepted attempt. A reservation interrupted
before its attempt is committed is likewise not automatically reused. These cases require a
future operator/provider-resolution procedure; this wave intentionally exposes no force-clear
or automatic retry escape. Restoring an older database requires separate writer fencing.

Clones share one fail-fast admission permit. Operations continue after cancellation of their
async waiter; each network/source phase has an explicit deadline. Independent handles are
fenced from repeated PUT by SQL reservation/attempt uniqueness, not by the local semaphore.
Once acceptance is recorded, only reads/deletes remain, so concurrent recovery does not create
new writes. No remote exclusivity guarantee is inferred from the local-filesystem backend.

Memory use includes one configured source buffer, bounded upload-channel chunks and the HTTP
parser/body buffer (HTTP/1 read-buffer limit8192). It never buffers a whole attachment or a
provider error body. The source is read twice and the remote file once to verify integrity;
this has I/O/bandwidth cost. Each backend owns a small reusable client pool with at most one
idle connection per host. A future scheduler must bound the number of resources/active handles.
These are implementation bounds, not measured process RSS or Bunny billing estimates.

## Cleanup and retention

Case removal hides/cancels ownership and clears the attempt's expected checksum as well as
journal origin/hash fields. Closure denies normal access immediately; bounded
`cancel_closed_evidence` preparation clears retained metadata for closed memberships.
Accepted cancelled captures can settle under cleanup; unaccepted attempts stay blocked.

Only cleanup-pending or erased evidence reaches
[DELETE](https://bunny.net/docs/api-reference/storage/manage-files/delete-file). HTTP200/404
must be followed by authenticated GET404 before SQL acknowledges absence. Failed deletion,
verification or SQL acknowledgment remains retryable; no PUT is retried. Cleanup addresses
only the immutable owned object. Other evidence, the marker and backups remain untouched.
After acknowledgment, closed-case purge deletes its owned transfer row atomically. There is
no automatic age expiry and no claim of backup-copy erasure or complete guild purge.

## Schema and validation

Guild schema7 adds a bounded attempt row per reserved object, an alias index and lifecycle
triggers. No URLs with credentials, source content or provider response bodies are stored.
The nonsecret regional zone binding and expected hash support recovery. Versions1–6 of the
verified guild family upgrade atomically without inventing attempts for existing evidence;
open requires7 and older guild-store binaries refuse7. Rollback needs matching backups and
binaries. Core remains0010, so current service startup/data behavior is unchanged.

Loopback HTTP tests cover bytes/checksums, source bounds/change, independent duplicate capture,
late writes after timeout, lost acceptance, corrupt reads, caller cancellation, identity/path/
redirect checks, cleanup failures and unrelated-object preservation. SQL tests cover guarded
transitions, scope, immutable attempts, checksum scrubbing, receipt-free acceptance retries,
version6 upgrade and rollback. CI also runs a separate disposable libSQL test for lost
reservation/start/acceptance/settlement/removal/deletion/purge commit responses with local HTTP.

These fixtures exercise protocol handling; they do not establish live Bunny durability,
replication visibility, privacy settings, error-response guarantees or resource consumption.
Live provider validation remains deferred, and runtime activation remains gated on the
installation, authorization, lifecycle and operational recovery work described above.
