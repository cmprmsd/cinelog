# Schema migrations

`../schema.sql` and `../meta_schema.sql` are the full current schemas; an empty database
gets them directly. A database of an older version is upgraded by the steps listed in
`INDEX_MIGRATIONS` / `META_MIGRATIONS` (`../schema.rs`), one file per version:

- `<db>/NNNN_<what>.sql`: the step **to** version `NNNN`. It must leave exactly the schema
  of the full file (`upgrades_match_fresh_schema`), so a new column goes at the end of its
  table in both.
- `<db>/vN.sql`: the full schema as released in version `N`, kept for that test.

Steps only move forward and are never edited after a release (docs/STABILITY.md).
