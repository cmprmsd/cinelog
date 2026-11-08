# Schema migrations

`src/schema.sql` (index.db) and `src/meta_schema.sql` (meta.db) always hold the **full current
schema**; `cinelog_index::schema::migrate` applies them to an empty database and sets
`PRAGMA user_version` and `PRAGMA application_id`.

Upgrades live here, one file per version step:

```
migrations/index/v0002.sql   index.db 1 → 2
migrations/meta/v0002.sql    meta.db  1 → 2
```

When a schema changes:

1. Edit the full schema file and bump `INDEX_SCHEMA_VERSION` / `META_SCHEMA_VERSION`.
2. Add the step file here and list it in `INDEX_MIGRATIONS` / `META_MIGRATIONS`
   (`Migration { to: N, sql: include_str!("../migrations/index/v000N.sql") }`).
3. Keep the released full schema of the previous version for the upgrade test
   (`released_schema` in `src/schema.rs`), e.g. as `migrations/index/released-v0001.sql`.

The tests check that the steps are contiguous and that upgrading from every released version
yields exactly the fresh schema. index.db is derived data, but it is still migrated in place
so that a version bump does not force a full re-index of a large corpus; meta.db is
authoritative and must always migrate.

Version 1 is the first release: there are no step files yet.
