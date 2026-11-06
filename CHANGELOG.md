# Changelog

## Unreleased

- Bound DNS resolution and connection setup for Gateway and REST. On Linux GNU,
  select the native resolver or supervised resolver helpers with `DFOR77_DNS_MODE`.
- Bound event queues and snapshot caches by memory as well as entry count, and
  report event continuity loss when the queue cannot retain normal events.
- Bound the Bot supervisor's IPC response writes and child exit waits, and reap
  managed children when the supervisor stops.
- Discard invalidated Gateway Resume state and its saved checkpoint, reject
  late dispatches after invalidation, and stop pumping closed or fatal sessions.
- Bound saved-session reads to 64 KiB, reject linked and special input files on
  Unix, and separate session persistence from the CLI whilst preserving atomic
  owner-only writes and valid Resume checkpoints.
- Add standard FORTRAN77 `D7PAV` poll-answer voters with bounded pagination and
  `D7EPL` poll expiry, usable with or without the extension.
- Include the poll message ID in audit targets without confusing answer IDs or
  pagination cursors with target users.
- Sanitise Gateway metadata and CLI diagnostics before writing to stderr,
  including Unicode line separators and directional controls, and protect
  metadata field boundaries.
- Redact Authorization headers containing horizontal tabs; escape display
  controls losslessly in audit, runtime and CLI diagnostic JSON.
- Share diagnostic rendering, redaction and JSON escaping in an internal text
  module whilst preserving existing public helpers and network JSON encoding.
- Share a five-per-twenty-second presence budget between typed and raw Gateway
  sends, whilst allowing voice requests and heartbeats to proceed.
- Validate raw Gateway envelopes before queueing, including integer opcodes and
  the data field, whilst retaining support for future opcode values.
- Bind queued Gateway requests to their session, wait for Resume completion and
  avoid replaying ambiguous send attempts. Failed queue audits leave requests
  unqueued without discarding previously accepted requests.
- Separate Gateway send handling and send budgets into internal modules whilst
  preserving existing public type paths; add `check_and_record_presence`.
- Reconcile guild departures and message deletions with cached permissions and
  messages, including bulk deletion and uncertain outcomes, whilst preserving
  stable indices in existing message searches.
- Invalidate message search results and message permission hints immediately on
  GAPs and non-resumable Gateway closes, without waiting for another frame.
- Discard snapshots and queued normal events when a Gateway session is replaced
  or a restored cursor skips events; preserve monotonic cursors and known Bot
  identities when restoring the current session.
- Reject conflicting Bot identities within an existing Gateway session.
- Split runtime permission checks and REST mutation reconciliation into internal
  modules without changing the public API.
- Revalidate affected permission caches after committed or uncertain guild,
  role, member, channel and thread-membership writes, including ownership,
  overwrite synchronisation and inferred private-thread access.
- Revalidate current and queued message permissions after relevant guild, role
  or member changes, successful REST refreshes, GAPs and permission expiry.
- Keep Gateway sequence cursors monotonic within each session and ignore stale
  or duplicate dispatches before updating snapshots or delivering Bot events.
- Invalidate cached permissions when an incoming Gateway frame cannot be
  normalised, whilst preserving the last accepted Resume cursor.
- Add standard FORTRAN77 `D7GRU` reaction-user retrieval with normal and burst
  types, bounded page sizes, validated Snowflake cursors and encoded emoji.
- Add `D7RNM` and `D7RBS` reaction-type constants to the extension include.
- Reject path-segment injection through IDs and tokens in FORTRAN typed REST
  helpers whilst preserving resource aliases and query builders.
- Add standard FORTRAN77 `D7CMD` command payloads, `D7B64` binary encoding,
  and `D7URI` data URI generation without requiring the extension.
- Add the `D7X64` in-memory data URI helper and share command and URI validation
  between standard and extension builders.
- Support UTF-8 character limits for command names and descriptions, and up to
  25 options in `D7SCX`.
- Generate context menu commands without an unsupported description field.
- Validate canonical base64 and media types before building data URIs.
- Preserve existing open units during read-only file conversion.
- Preserve query builder errors when the internal buffer is exceeded.
- Update cached image counts on message edits whilst preserving omitted fields,
  and share attachment classification between Gateway and REST snapshots.
- Add optimised, bounds-checked FORTRAN builder regression tests.

## 0.3.0

- Initial public development preview.
- Provides a Rust Discord runtime and fixed-form FORTRAN77 `D7` wrapper.
- Separates standard `dfor-77` from `dfor-77 extension`.
- Includes source policy checks, mock Gateway tests, REST checks, and staging gates.
- Prepares the public repository tree for `origin/master`.
- Splits developer documentation into `DEVELOPMENT.md`.
- Adds a separate README for the sample Bot.
- Keeps live staging files, logs, and operational secrets outside the public tree.
- Keeps the published crate file list fixed to the runtime, FORTRAN wrapper, extension files, configuration samples, and top-level release documents.
