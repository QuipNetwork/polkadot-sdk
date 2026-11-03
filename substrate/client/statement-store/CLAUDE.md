# CLAUDE.md

Guidance for working on `sc-statement-store`, in addition to the repository-level CLAUDE.md.

## Database consistency principle

The startup path (`Store::new`: `migrate_columns`, `check_db_version`, `populate`, including
the version migration `populate` may run) is the only place allowed to repair, rebuild, or
drop inconsistent database data. Once `populate` has returned, all code assumes the data it
reads from the database is consistent.

When code running after `populate` encounters data that violates this assumption — a
statement body that fails to decode, an index key or value that does not parse, an index row
whose statement body is missing under the submit-index write lock, or any other impossible
state — it must:

- log the finding with `log::error!` (target `statement-store`), identifying the offending
  item (typically by hash);
- skip the item and carry on;
- never attempt a repair: no deleting, rewriting, or recomputing of the broken data, even
  when a fix looks trivial. Repeated errors on every encounter are the intended behavior.

Rationale: inconsistent data after startup means a bug in the store or external interference
with the database. Silently "healing" it would mask the bug; the error log is the signal to
investigate (and, for an operator, to wipe the statement database, which re-syncs from
gossip).

Two things are explicitly *not* inconsistencies:

- A missing statement body on a lock-free read path (queries, gossip reads): reads race with
  concurrent removals, so an index scan may yield a hash whose body is already gone. This is
  benign and logged at `debug` at most. Under the submit-index write lock no mutation can
  race, so the same mismatch there *is* an inconsistency.
- Database I/O errors (failed `parity_db` reads/writes/commits): these are operational
  errors with their own handling and log levels, not data corruption.
