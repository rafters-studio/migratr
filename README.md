# migratr

## CLI

```
migratr [--db PATH] [--dir PATH] [--json] <new|up|down|status|plan|restore> ...
```

`--dir` defaults to `migrations`. `--db` is required by every command except `new`.

| Command | What it does |
|---|---|
| `new <name> [specs...]` | Scaffold a migration file. |
| `up [--to V]` | Apply pending migrations. |
| `down [--steps N]` | Reverse the newest N applied migrations (default 1). |
| `status` | List each migration as applied (with time) or pending, checksum mismatches, missing files, and the latest snapshot. |
| `plan [up [--to V] \| down [--steps N]]` | Print the statements a run would execute, which steps are destructive, the snapshot each would take, and the database size. Changes nothing. |
| `restore [--snapshot PATH] --yes` | Replace the database with a snapshot. Without `--yes` nothing changes. |

After every successful `up` and `down`, migratr writes `schema.json` in the migrations directory.

### JSON output

With `--json`, a command writes one JSON document to stdout and nothing else; logs go to stderr.
The document carries `command` and `status` (`ok` or `error`). A success adds the outcome's fields.
A failure adds `code` and `message` and the process exits non-zero (2 for a usage error, 1 otherwise).

### Error codes

Codes are stable across patch releases.

| Code | Cause |
|---|---|
| `usage` | Bad arguments, or a missing `--db`. |
| `parse` | A migration file or spec could not be parsed. |
| `duplicate_version` | Two migration files share a version. |
| `io` | A file or directory could not be read or written. |
| `checksum_mismatch` | An applied migration's file was edited. |
| `missing_file` | A ledger row has no migration file. |
| `apply_failed` | A migration failed and was rolled back. |
| `irreversible` | A migration in the `down` range has an operation with no inverse. |
| `unparseable_table` | A table's `CREATE TABLE` could not be parsed for a rebuild. |
| `foreign_key_violation` | A table rebuild left rows that violate foreign keys. |
| `column_in_use` | A dropped column is used by another object. |
| `rebuild_not_first` | An operation that needs a table rebuild is not first in its migration. |
| `snapshot_failed` | A snapshot could not be written, so the step did not run. |
| `restore_not_confirmed` | `restore` ran without `--yes`. |
| `unknown_object` | `new` named an object that is not in `schema.json`. |
| `database` | SQLite reported an error. |
