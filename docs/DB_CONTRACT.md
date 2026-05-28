# SQLite Database Contract

## Schema Ownership

The `musefs` scanner owns the `tracks` table. External tools (e.g. the Beets
plugin) must use the scanner (`musefs scan --db <path> ...`) to add, update, or
remove structural rows.

### Scanner-Owned Fields (read-only for external tools)

- `tracks.id` — Auto-generated primary key
- `tracks.backing_path` — Set by scanner on import
- `tracks.audio_offset` — Computed by format analysis
- `tracks.audio_length` — Computed by format analysis
- `tracks.backing_size` — File metadata at scan time
- `tracks.backing_mtime` — File metadata at scan time
- `tracks.content_version` — Bumped by triggers only
- `tracks.updated_at` — Managed by SQLite

### External-Writer Allowed Fields

- `tags` — Full read/write: track metadata tags
- `track_art` — Full read/write: cover art links
- `art` — Full read/write (content-addressed by sha256): image blobs

### Constraints

- Foreign keys: `tags.track_id` → `tracks.id`, `track_art.track_id` → `tracks.id`,
  `track_art.art_id` → `art.id`
- `ON DELETE CASCADE` on `tracks` → `tags`, `track_art`
- Triggers bump `tracks.content_version` on tag/art changes
