# Architecture

## Localisation system

The loc system resolves `$KEY$` references in game script files to their
translated text (shown in hover tooltips) and validates that referenced keys
actually exist (CW100/CW122).

### Data flow

```
.yml files on disk
       │
       ▼
  LocService (parses .yml → Vec<LocFile>)
       │
       ├──► LocIndex (lowercased key sets, per-language)
       │         - exists_any(key) — does this key exist in any language?
       │         - missing_synced_languages(key) — which languages lack it?
       │
       └──► loc_text map (HashMap<key, Vec<(Lang, text)>>)
                 - used by hover to show translations
                 - rebuilt during workspace scan
```

### Current implementation

All loc data lives in memory:

- **`LocService`** — owns every parsed `LocFile` (the full AST of every `.yml`
  file). Built from disk during the workspace scan. Dropped after the index is
  built to free memory (~2M entries on Millennium Dawn).
- **`LocIndex`** — lowercased key sets per language + a union set. Built from
  `LocService`, then the service is dropped. Answers existence queries for
  config validation.
- **`loc_text`** — `HashMap<String, Vec<(Lang, String)>>` for hover display.
  Built from `LocService` before it's dropped. Only rebuilt during the full
  workspace scan.
- **`loc_live_overlay`** — per-open-file key sets for incremental `$ref$`
  checks. Updated on every loc file edit so newly-added keys resolve
  immediately without a full rescan.

### Recent fixes

| # | Bug | Fix |
|---|---|---|
| 50 | Inline `#` comments shown in loc tooltip | `strip_loc_comment()` strips `#` after the closing `"` |
| 51 | Vanilla loc keys missing from hover | Always load vanilla loc files for the hover text map, even when a cache is present |
| 52 | Goto-def points to old location after file move | `did_change_watched_files` handler clears deleted files from all indexes |
| 53 | Loc tooltip stale after edit | `update_loc_text_for_file()` re-parses the edited file and merges entries into `loc_text` on every edit |

### Planned: SQLite-backed loc index

The in-memory HashMap approach works but has limitations:

1. **Full rebuild on every scan** — all `.yml` files are re-parsed even if
   nothing changed. A persistent DB would skip unchanged files.
2. **No incremental file tracking** — the `loc_text` map can't track which keys
   came from which file, so removing a file's contributions requires a full
   rebuild.
3. **Memory pressure** — the full loc AST (~2M entries) is held until the
   index is built, then dropped. A DB could stream results.

The planned migration replaces the `loc_text` HashMap with a SQLite database:

```
loc_entries table:
  key       TEXT NOT NULL,   -- lowercased loc key
  lang      TEXT NOT NULL,   -- language name (english, french, …)
  text      TEXT NOT NULL,   -- display text (quotes + comments stripped)
  file_path TEXT NOT NULL,   -- source file for incremental removal
  line      INTEGER,         -- source line for goto-def
  PRIMARY KEY (key, lang, file_path)
```

Benefits:

- **Incremental updates**: `INSERT OR REPLACE` on edit, `DELETE WHERE file_path = ?` on close
- **Consistent state**: single source of truth, no stale entries
- **Lazy loading**: query only the keys needed for hover, not the full set
- **Persistence**: survives server restart (warm cache)

The `LocIndex` (key existence checks) would remain in memory for speed — it's
just a `HashSet<String>` (~2M entries, ~50MB) and is cheap to rebuild. Only
the hover text map moves to SQLite.

## Build system

See `BUILD.md` for build instructions and `PROFILING.md` for build/runtime
profiling.
