# Security Policy

## Supported Version

| Version | Status |
| --- | --- |
| 0.3.x | Development preview |

## Reporting

Report security issues privately to the repository maintainer before public disclosure. Do not include Discord Bot tokens, audit hash keys, private guild data, or message content in a public issue.

## Operational Notes

- Store Discord Bot tokens only in environment variables.
- Environment variables with the prefix DFOR77_ and suffix _KEY are reserved
  for runtime credentials and are removed from the Bot child environment.
- Use `audit.content_mode = "none"` or `hash` for production-like runs.
- Protect any private control or diagnostic endpoints with a high-entropy secret.
- Treat audit logs, runtime logs, and stderr logs as operational records that must not be published.
- Runtime and CLI stderr diagnostics share credential redaction and control-character handling.
  Unicode line separators and directional formatting characters are rendered
  visibly; untrusted metadata fields also encode separators that could forge
  neighbouring fields. Normal Unicode text and emoji joiners remain available.
  Authorization headers containing horizontal tabs are redacted as complete
  headers. Audit, runtime and CLI diagnostic JSON escape display controls whilst
  preserving the original values when decoded; network JSON encoding is
  unchanged.
- REST request, response, upload, and outcome diagnostics omit paths and payloads.
  IPC fields, snapshots, Gateway frames, sessions, transports, and runtime debug
  output expose metadata and sizes rather than authentication or message data.
  Queue and cache diagnostics report counts and budgets without expanding their
  entries. Explicit payload accessors and session serialisation retain the data
  required for operation and must not be used as public diagnostic output.
- Keep upload directories separate from source, config, and secret files when using `D7UPL` or `D7UPM`.
- Run staging verification in a dedicated guild before connecting to production guilds.

## Input and resource limits

- Typed poll helpers validate channel, message and cursor IDs as positive
  unsigned 64-bit Snowflakes before emitting IPC. Voter page sizes are limited
  to 1–100; non-negative answer IDs are not restricted to a presumed sequence.
  Poll expiry uses the normal uncertain-write policy and is not automatically
  retried. Audit targets identify the channel and poll message without treating
  answer IDs or cursors as affected users.
- JSON parsing rejects inputs over 16 MiB, nesting beyond 64 containers, more than
  262,144 values, duplicate object keys, and trailing commas. Permission decimal
  strings are limited to 128 digits.
- Gateway connections carrying tokens use `wss://` on port 443 under
  `.discord.gg`. Plain WebSockets are restricted to loopback mock servers.
  Redirects are not followed. Incoming messages and frames are limited to 16 MiB.
- Gateway dispatches cannot rewind the accepted sequence within a session.
  Stale and duplicate dispatches do not update snapshots or reach Bot event
  handlers. Resume completion preserves the highest accepted cursor, and a new
  READY session may start at a lower sequence. Malformed present sequence values
  are rejected; frame normalisation failures invalidate snapshots and report a
  GAP without advancing the Resume cursor.
- Non-resumable disconnects, fresh Identify handshakes and explicit shutdowns
  discard the old Resume state. Late dispatches cannot restore it; a new READY
  or an explicit validated restore is required. Resumable disconnects preserve
  their session and cursor.
- Saved Gateway sessions are limited to 64 KiB before JSON parsing, including
  a bounded read if the file grows. On Unix, descriptor checks reject symbolic
  links, hard links and non-regular files without blocking on a FIFO. A known
  invalidated session removes its checkpoint, including after graceful
  shutdown; an uninitialised runtime leaves an existing checkpoint untouched.
- Queued user Gateway payloads retain their originating session and are sent
  only when that session is ready. Normal Resume keeps pending requests; a
  different session cannot receive them. Attempted sends are removed before
  entering the transport, so an ambiguous send failure cannot automatically
  replay the request. Queue audit failures do not leave failed requests queued.
- Presence updates share a five-per-twenty-second sliding budget across typed
  and raw sends, in addition to the connection's general send budget and
  heartbeat reservation. Reconnection does not reset the presence window.
  Deferred presence updates do not block queued voice requests or heartbeats.
  Raw envelopes require a non-negative integer opcode and a data field, so
  alternate numeric representations cannot bypass presence classification.
- Replacing a Gateway session discards snapshots, pending normal events and
  cached permission hints before the new session can use them. A missing Bot
  identity in the new session does not inherit the old identity. Restoring the
  current session cannot rewind its cursor or change a known Bot identity;
  advancing it beyond received events reports a GAP and invalidates cached
  state. Conflicting identities in READY also invalidate state without changing
  the session identity or cursor. GAPs also discard message search results;
  non-resumable closes apply these invalidations and notify clients immediately,
  without waiting for another Gateway frame. Normal Resume completion preserves
  snapshots and existing search results.
- Message-provided permission hints expire with the member snapshot TTL.
  Guild and role changes invalidate hints for that guild; member changes
  invalidate hints for that member. These invalidations cover both the current
  message and queued messages. Successful REST member and role refreshes, known
  member removal, GAPs and protocol failures also discard affected hints before
  administrator checks can reuse them. Fresh hints for unrelated guilds or
  members remain available.
- Committed or uncertain REST writes to guilds, roles, members and bans invalidate
  affected permission snapshots and message hints before the next check.
  Guild ownership is revalidated; role deletion also invalidates member
  assignments. Channel changes, permission overwrites, position synchronisation
  and Bot thread-membership changes invalidate affected channel snapshots.
  Changes to Bot permissions or channel access discard inferred private-thread
  access. Unsent and rejected writes retain their existing caches; uncertain
  writes are not automatically retried. Permission updates preserve message
  snapshots and unrelated guilds. Confirmed guild or channel deletions mark
  those resources as deleted and remove their cached messages. Leaving a guild
  through the current-user endpoint follows the same invalidation rules.
  Uncertain deletions permit fresh lookups instead of claiming that the
  resource is already gone.
- Confirmed single and bulk message deletions remove affected snapshots and
  record deletion tombstones. Uncertain deletions discard affected positive
  snapshots without asserting that the messages are gone. An uncertain bulk
  deletion whose target IDs cannot be parsed invalidates that channel's message
  snapshots; known deletion tombstones remain intact. New message searches
  reflect these changes, whilst existing search indices remain stable for
  clients iterating through a result set. Reaction and pin removals do not
  delete message snapshots.
- DNS, TCP, TLS, request writes, and response reads share a deadline. At most four
  DNS resolver workers may remain active; a resolver exceeding its deadline does
  not block the caller. Gateway send and receive calls renew their own deadline.
- On Linux with GNU libc, DNS uses `/usr/bin/prlimit` and `/usr/bin/getent` when
  both helpers are installed. The helpers retain system NSS lookup, IPv4 and
  IPv6, search domains, host aliases, and selected NSS configuration variables.
  Only DNS, CA, Kerberos and LDAP settings, plus `HOME`, are forwarded; do not
  store credentials in these configuration variables. Loader overrides are
  omitted. The core-file
  limit is zero, output is bounded to 32 KiB, and at most 16 TCP results are used.
  Deadline, output-limit, and result-limit paths kill and reap the direct helper.
  Native resolver faults, including the conditions described in
  [CVE-2026-8674](https://www.openwall.com/lists/oss-security/2026/09/17/4),
  become failed lookups rather than runtime crashes on this path. Helpers share
  the runtime's user and filesystem access. `DFOR77_DNS_MODE=process` requires
  both helpers and fails lookups if either is missing. The default, `auto`, uses
  the native resolver if helpers are missing; `native` explicitly retains the
  original resolver for custom in-process NSS integrations. Invalid mode values
  fail lookups. These settings apply to GNU Linux hostname lookups; numeric IPs
  and `localhost` bypass DNS. Other platforms retain the native resolver. Native
  resolution requires OS security fixes.
- REST response headers are limited to 64 KiB and responses to 16 MiB. Conflicting
  framing headers are rejected. A partially written request is treated as an
  unknown commit, so callers must not blindly retry a mutating operation.
  Informational response heads share the header budget and are discarded before
  the final response. Chunked responses are decoded incrementally; chunk-size
  lines and the trailer section are each limited to 64 KiB. Malformed chunk
  extensions, framing trailers, bare LF lines, and whitespace in field names
  are rejected. Trailer fields do not replace the final response headers.
- FORTRAN helpers constructing typed REST paths validate each ID and token as
  one printable ASCII path segment before emitting IPC. Slashes, backslashes,
  percent escapes, query and fragment delimiters, and `.` or `..` segments are
  rejected. Existing aliases such as `@me` and `@original` remain available.
  `D7RST` remains the explicit low-level interface for caller-supplied paths.
- Audit and runtime logs rotate at 64 MiB each, retaining two archives named
  `.1` and `.2`. Archive records remain private operational data. Rotation bounds
  local storage and is not a substitute for an external retention policy.
- On Unix, log files are opened without following a final symlink or blocking
  on a FIFO. File type and link count are checked on the open descriptor before
  permissions or contents are changed. Keep log and session directories private.
- The supervisor kills and reaps its direct Bot child on failure. Bot shutdown
  after IPC closes has a deadline; Unix IPC response writes also share one
  deadline per response. `--bot-io-timeout-ms` configures these waits and defaults
  to 10,000 ms. Unix request reads are cancelled at shutdown even when another
  process retains the Bot's output descriptor. Other platforms retain their
  existing anonymous-pipe transport.
- A fatal shard failure stops the other shards and waits for their Bot children
  before the supervisor exits. Pending starts and restart delays are cancelled.
- The supervisor removes configured token and audit-key environment variables,
  and recognised infrastructure credentials, before launching the Bot. This is
  defence in depth: the Bot still shares the runtime's user and filesystem and
  can invoke authorised `D7` operations. It is not an untrusted-code sandbox.

Permission snapshots validate role IDs, role positions, and overwrite kinds.
Duplicate roles or overwrite keys are incomplete state rather than an ordering
rule. Malformed known permission dispatches produce an
`INVALID_PERMISSION_DISPATCH` GAP and invalidate cached snapshots; unrelated
future dispatches remain available as generic events. REST snapshots must match
the requested resource identity. Administrator permission does not bypass the
timeout role-hierarchy policy or make protected message types deletable.
Role and overwrite membership checks use indexes to avoid quadratic work on
large payloads.

Channel overwrites cannot grant or revoke known guild-only permissions, including
`ADMINISTRATOR`. Guild ownership and role-based administrators retain their
overwrite bypass. The `@everyone` overwrite is applied once, separately from
assigned roles, even if a provider includes the default role in a member's role
list. Unknown permission bits retain their existing behaviour for compatibility
with future Discord permissions. Applicable overwrite scopes are listed in the
[permission reference](https://docs.discord.com/developers/topics/permissions).

Individual role changes do not renew the age of the full role list. Role
deletions remove assignments from cached members of that guild without renewing
their other state. Guild updates that omit the owner retain the previous
ownership age and completeness. Expired or incomplete ownership is confirmed
through the [Get Guild endpoint](https://docs.discord.com/developers/resources/guild#get-guild)
before granting owner privileges; failed confirmation cannot grant them. Guild
identity, owner IDs, and availability flags are validated. Ownership changes
invalidate cached private-thread access.

Member timeout timestamps are validated and re-evaluated against their expiry
when cached members are read. Fractional milliseconds round up to avoid ending
a timeout early. Partial member updates retain omitted roles and timeout state;
an explicit null clears a timeout. A timeout-only update does not renew the age
of cached roles. Missing or invalid permission data remains incomplete until it
can be refreshed.

Partial channel and thread updates retain omitted overwrites, parent links,
archive state, and lock state. Only an update with all cached permission fields
renews the channel's age or completes a partial record. Malformed present fields
are rejected. Gateway channels carrying `CHANNEL_OBFUSCATED` are unavailable for
send and delete preflights, including with stale administrator permissions.
Clearing that flag alone does not make the old metadata complete.

Thread send and delete checks use the parent's current permission overwrites;
stale or incomplete parent data must be refreshed. Parent resolution is limited
to one hop and validates guild identity and compatible channel types. Private
threads require membership or effective `MANAGE_THREADS` permission. Deleting
messages in a locked, archived thread also requires `MANAGE_THREADS`. A missing
parent or missing archive or lock state makes a parsed thread incomplete.
Thread membership probes revalidate refreshed channel restrictions and preserve
newly observed membership rather than overriding it with an earlier 404.

Archived private-thread pages validate their thread, parent, guild, and current
user identities. Joined-thread lookup uses the descending ID cursor to fetch a
target beyond the first page with one additional request. Moderator lookup can
refresh the exact channel rather than walking an unbounded archive. Malformed
pages and member responses leave membership unknown. Page helpers validate and
encode Snowflake or timestamp cursors without changing REST bucket grouping.

Gateway and REST message snapshots share field validation. Malformed message
dispatches report `INVALID_MESSAGE_DISPATCH`; conflicting cached message
identities report `MESSAGE_IDENTITY_CONFLICT`. Both invalidate snapshots. Late
updates preserve deletion tombstones. Message searches enforce snapshot TTLs,
and partial edits do not renew the age of omitted fields. A complete update can
refresh the snapshot. Role changes discard older computed permissions unless
the update supplies new permissions. Attachment classification keeps MIME and
filename fallbacks without allocating lowercase copies of untrusted strings.

Normal event queues have an additional 16 MiB retention budget by default,
configured with `snapshot.event_queue_max_bytes`. Control notifications have
eight reserved slots, with bounded GAP reasons. Count or byte overflow discards
pending normal events and reports a GAP; every loss invalidates snapshots even
when notifications are coalesced. A GAP is not proof that every event was moderated.

Snapshot caches share a 64 MiB retention budget by default, configured with
`snapshot.cache_max_bytes`. Existing entry-count limits still apply. Eviction
removes the oldest entry from the largest cache; a single oversized entry is
discarded without flushing unrelated entries. Missing permission snapshots must
be refreshed or produce an incomplete-state result. Accounting includes retained
string and vector capacities and index overhead estimates. These budgets do not
include allocator overhead, transient parsing, or the Bot process, and are not
process RSS limits.

## Native TLS dependencies

`cargo audit` checks Rust advisories; it does not audit the system OpenSSL library
used by `native-tls` on Linux. Maintain a supported, patched operating system and
check its security tracker separately. For example,
[CVE-2026-35189](https://openssl-library.org/news/vulnerabilities-3.0/)
can consume excessive memory while processing a peer certificate, before an
application's JSON limits apply. Check the distributor's backported fixes rather
than comparing upstream version strings alone.
