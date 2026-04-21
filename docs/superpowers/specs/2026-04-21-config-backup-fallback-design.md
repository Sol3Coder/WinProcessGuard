# Config Backup Fallback Design

**Goal**

Make the service recover configuration from `config_bak.json` when `config.json` is missing, empty, unreadable, or contains invalid JSON, while never modifying `config_bak.json` and never crashing if the backup is also unavailable.

**Scope**

This change is limited to the Rust service under `process-guard-service`. It updates configuration loading behavior and adds regression tests for the fallback path. It does not change IPC, monitor semantics, or backup file maintenance; `config_bak.json` remains externally managed.

## Current Context

- Configuration loading is implemented in `process-guard-service/src/config.rs`.
- `load_config()` currently:
  - creates a default `config.json` when the main file is missing
  - returns a default config when the main file is empty
  - returns a default config when reading or parsing fails
- There are no tests for the startup config recovery path.

## Requirements

### Functional

1. When startup reads `config.json` successfully, keep the existing behavior.
2. When `config.json` is missing, empty, unreadable, or invalid JSON:
   - attempt to load `config_bak.json`
   - treat the backup as read-only input
   - if the backup loads successfully, write the loaded content back to `config.json`
3. If `config_bak.json` is missing, empty, unreadable, or invalid JSON:
   - do not crash
   - continue running with `Config::new()`
4. If recovered config contains duplicate `exe_path` values, preserve the existing deduplication behavior and sync the resulting normalized config to `config.json`.

### Non-Functional

1. `config_bak.json` must never be written, truncated, renamed, or otherwise modified by the program.
2. Logging should clearly indicate whether startup used:
   - primary config
   - backup config
   - default config due to both sources being unusable
3. Failures in both config files must degrade to normal service startup with an empty configuration.

## Design

### File Responsibilities

- `process-guard-service/src/config.rs`
  - owns config path resolution
  - owns config file parsing helpers
  - owns main-to-backup fallback policy
  - owns syncing recovered config back to `config.json`
- `process-guard-service/src/models.rs`
  - defines `CONFIG_BACKUP_FILE_NAME` beside the existing `CONFIG_FILE_NAME`

### Load Strategy

`load_config()` will become policy-driven:

1. Resolve `config.json` and `config_bak.json` paths.
2. Try loading `config.json`.
3. If the result is usable, return it after existing normalization.
4. If the main config is not usable because it is missing, empty, unreadable, or invalid:
   - log the main-config failure reason
   - try loading `config_bak.json` in read-only mode
5. If the backup is usable:
   - normalize it with the existing deduplication path
   - write the normalized result to `config.json`
   - return the normalized config
6. If the backup is also unusable:
   - log that startup is falling back to `Config::new()`
   - return default config without panicking

### Parsing Helper Boundary

Add a small internal helper that loads a specific path and returns either:

- parsed `Config`
- a categorized failure reason for:
  - missing file
  - empty file
  - read failure
  - parse failure

This keeps `load_config()` readable and makes tests focus on behavior instead of string matching.

### Writeback Rules

- Only `config.json` is ever written by automatic recovery.
- Successful backup recovery writes the recovered config to `config.json`.
- Backup recovery failure does not write a new `config.json`.
- The service writes `config.json` only when:
  - a valid main config is normalized and deduplication needs persistence, or
  - a valid backup config is recovered and synced to main

### Error Handling

- `load_config()` remains non-panicking and returns `Config`.
- Any filesystem or JSON error is converted into logs plus fallback behavior.
- `ensure_config_dir()` failure should not panic the service; it should remain best-effort as it is now.

## Test Design

Add unit tests in `process-guard-service/src/config.rs` or a dedicated config test module that run against temporary directories by injecting/overriding config paths.

Required cases:

1. Missing `config.json` + valid `config_bak.json`
   - loads backup
   - writes equivalent content to `config.json`
2. Empty `config.json` + valid `config_bak.json`
   - loads backup
   - writes equivalent content to `config.json`
3. Invalid `config.json` + valid `config_bak.json`
   - loads backup
   - writes equivalent content to `config.json`
4. Missing or unusable main + missing backup
   - returns empty default config
   - does not panic
5. Missing or unusable main + empty backup
   - returns empty default config
   - does not panic
6. Missing or unusable main + invalid backup
   - returns empty default config
   - does not panic
7. Valid backup file remains byte-for-byte unchanged after recovery
8. Backup with duplicate `exe_path`
   - returned config is deduplicated
   - deduplicated content is synced to `config.json`
   - backup remains unchanged

## Risks And Mitigations

- Risk: current path helpers depend on the executable directory, which is awkward for tests.
  - Mitigation: extract an internal path-based loader used by both production code and tests.
- Risk: deduplication currently writes via `save_config()`, which could accidentally target the wrong file in tests if path injection is incomplete.
  - Mitigation: make the internal loader/save path explicit in tests.
- Risk: backup parsing could silently mask a corrupted main config.
  - Mitigation: keep explicit warning/error logs identifying both the main failure and the recovery source.

## Acceptance Criteria

- Startup no longer loses configured monitors when only `config.json` is missing, empty, or malformed but `config_bak.json` is valid.
- Startup does not crash when both `config.json` and `config_bak.json` are unusable.
- `config_bak.json` is never modified by automatic recovery.
- Automated tests cover the fallback matrix and pass.
