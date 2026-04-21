# Config Backup Fallback Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make startup recover `config.json` from `config_bak.json` when the main config is missing, empty, unreadable, or invalid, without ever modifying the backup file or crashing when both files are unusable.

**Architecture:** Keep the change inside the config module by introducing a path-based loader that can classify main and backup load failures. `load_config()` remains the production entry point, while tests exercise the fallback policy against temporary files and verify that only `config.json` is written during recovery and deduplication.

**Tech Stack:** Rust 2021, `serde`, `serde_json`, std `fs`/`io`, `cargo test`

---

### Task 1: Add Explicit Backup File Naming And Path Helpers

**Files:**
- Modify: `process-guard-service/src/models.rs`
- Modify: `process-guard-service/src/config.rs`
- Test: `process-guard-service/src/config.rs`

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn backup_config_path_uses_config_bak_json_name() {
    let dir = std::path::PathBuf::from(r"F:\temp\pg-config-test");
    let path = get_config_backup_file_path_for_tests(&dir);
    assert!(path.ends_with("config_bak.json"));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test backup_config_path_uses_config_bak_json_name -- --exact`
Expected: `FAIL` because `get_config_backup_file_path_for_tests` and/or the backup file constant do not exist yet.

- [ ] **Step 3: Write minimal implementation**

```rust
pub const CONFIG_BACKUP_FILE_NAME: &str = "config_bak.json";
```

```rust
pub fn get_config_backup_file_path() -> PathBuf {
    get_config_dir().join(CONFIG_BACKUP_FILE_NAME)
}

fn get_config_backup_file_path_for_tests(base_dir: &std::path::Path) -> PathBuf {
    base_dir.join(CONFIG_BACKUP_FILE_NAME)
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test backup_config_path_uses_config_bak_json_name -- --exact`
Expected: `PASS`

- [ ] **Step 5: Commit**

```bash
git add process-guard-service/src/models.rs process-guard-service/src/config.rs
git commit -m "refactor: add backup config path helpers"
```

### Task 2: Add Failing Tests For Main-To-Backup Recovery

**Files:**
- Modify: `process-guard-service/src/config.rs`
- Test: `process-guard-service/src/config.rs`

- [ ] **Step 1: Write the failing test for missing main config recovery**

```rust
#[test]
fn missing_main_uses_backup_and_syncs_main() {
    let harness = ConfigTestHarness::new();
    harness.write_backup(
        r#"{"items":[{"id":"1","exe_path":"C:\\App.exe","args":null,"name":"App","minimize":false,"no_window":false,"enabled":true,"heartbeat_timeout_ms":10000}]}"#,
    );

    let config = load_config_from_paths(harness.main_path(), harness.backup_path());

    assert_eq!(config.items.len(), 1);
    assert!(harness.main_path().exists());
    let synced = std::fs::read_to_string(harness.main_path()).unwrap();
    assert!(synced.contains(r#""exe_path": "C:\\App.exe""#));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test missing_main_uses_backup_and_syncs_main -- --exact --nocapture`
Expected: `FAIL` because `load_config_from_paths` and the test harness do not exist yet.

- [ ] **Step 3: Write the failing test for empty main config recovery**

```rust
#[test]
fn empty_main_uses_backup_and_syncs_main() {
    let harness = ConfigTestHarness::new();
    harness.write_main("   ");
    harness.write_backup(valid_single_item_json());

    let config = load_config_from_paths(harness.main_path(), harness.backup_path());

    assert_eq!(config.items.len(), 1);
    let synced = std::fs::read_to_string(harness.main_path()).unwrap();
    assert!(synced.contains(r#""name": "App""#));
}
```

- [ ] **Step 4: Run test to verify it fails**

Run: `cargo test empty_main_uses_backup_and_syncs_main -- --exact --nocapture`
Expected: `FAIL` because fallback-on-empty has not been implemented.

- [ ] **Step 5: Write the failing test for invalid main config recovery**

```rust
#[test]
fn invalid_main_uses_backup_and_syncs_main() {
    let harness = ConfigTestHarness::new();
    harness.write_main("{invalid-json");
    harness.write_backup(valid_single_item_json());

    let config = load_config_from_paths(harness.main_path(), harness.backup_path());

    assert_eq!(config.items.len(), 1);
    let synced = std::fs::read_to_string(harness.main_path()).unwrap();
    assert!(synced.contains(r#""items""#));
}
```

- [ ] **Step 6: Run test to verify it fails**

Run: `cargo test invalid_main_uses_backup_and_syncs_main -- --exact --nocapture`
Expected: `FAIL` because parse-failure fallback has not been implemented.

- [ ] **Step 7: Write the failing tests for unusable backup degradation**

```rust
#[test]
fn missing_main_and_missing_backup_returns_default_without_panic() {
    let harness = ConfigTestHarness::new();
    let config = load_config_from_paths(harness.main_path(), harness.backup_path());
    assert!(config.items.is_empty());
    assert!(!harness.main_path().exists());
}

#[test]
fn missing_main_and_empty_backup_returns_default_without_panic() {
    let harness = ConfigTestHarness::new();
    harness.write_backup(" \n\t ");
    let config = load_config_from_paths(harness.main_path(), harness.backup_path());
    assert!(config.items.is_empty());
}

#[test]
fn missing_main_and_invalid_backup_returns_default_without_panic() {
    let harness = ConfigTestHarness::new();
    harness.write_backup("{broken");
    let config = load_config_from_paths(harness.main_path(), harness.backup_path());
    assert!(config.items.is_empty());
}
```

- [ ] **Step 8: Run tests to verify they fail**

Run: `cargo test returns_default_without_panic -- --nocapture`
Expected: `FAIL` because the path-based loader and graceful fallback behavior are not implemented yet.

- [ ] **Step 9: Write the failing tests for backup immutability and dedup sync**

```rust
#[test]
fn backup_file_remains_unchanged_after_recovery() {
    let harness = ConfigTestHarness::new();
    let backup = valid_single_item_json();
    harness.write_backup(backup);

    let _ = load_config_from_paths(harness.main_path(), harness.backup_path());

    let backup_after = std::fs::read_to_string(harness.backup_path()).unwrap();
    assert_eq!(backup_after, backup);
}

#[test]
fn duplicate_backup_entries_are_deduplicated_in_main_only() {
    let harness = ConfigTestHarness::new();
    let backup = r#"{
  "items": [
    {"id":"1","exe_path":"C:\\App.exe","args":null,"name":"App A","minimize":false,"no_window":false,"enabled":true,"heartbeat_timeout_ms":10000},
    {"id":"2","exe_path":"C:\\App.exe","args":null,"name":"App B","minimize":false,"no_window":false,"enabled":true,"heartbeat_timeout_ms":10000}
  ]
}"#;
    harness.write_backup(backup);

    let config = load_config_from_paths(harness.main_path(), harness.backup_path());

    assert_eq!(config.items.len(), 1);
    let main_after = std::fs::read_to_string(harness.main_path()).unwrap();
    let backup_after = std::fs::read_to_string(harness.backup_path()).unwrap();
    assert!(main_after.contains("App B"));
    assert_eq!(backup_after, backup);
}
```

- [ ] **Step 10: Run tests to verify they fail**

Run: `cargo test backup_ -- --nocapture`
Expected: `FAIL` because recovery and dedup writeback semantics are not implemented yet.

- [ ] **Step 11: Commit**

```bash
git add process-guard-service/src/config.rs
git commit -m "test: cover config backup fallback behavior"
```

### Task 3: Implement Path-Based Loader And Graceful Fallback Policy

**Files:**
- Modify: `process-guard-service/src/config.rs`
- Test: `process-guard-service/src/config.rs`

- [ ] **Step 1: Write minimal test support helpers**

```rust
struct ConfigTestHarness {
    root: std::path::PathBuf,
    main: std::path::PathBuf,
    backup: std::path::PathBuf,
}

impl ConfigTestHarness {
    fn new() -> Self {
        let unique = format!(
            "pg-config-tests-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let root = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&root).unwrap();
        Self {
            main: root.join("config.json"),
            backup: root.join("config_bak.json"),
            root,
        }
    }
    fn main_path(&self) -> &std::path::Path { &self.main }
    fn backup_path(&self) -> &std::path::Path { &self.backup }
    fn write_main(&self, content: &str) { std::fs::write(&self.main, content).unwrap(); }
    fn write_backup(&self, content: &str) { std::fs::write(&self.backup, content).unwrap(); }
}
```

- [ ] **Step 2: Run the first recovery test to confirm the helper compiles but behavior still fails**

Run: `cargo test missing_main_uses_backup_and_syncs_main -- --exact --nocapture`
Expected: `FAIL` with assertions about fallback behavior, not missing test symbols.

- [ ] **Step 3: Implement classified file loading**

```rust
#[derive(Debug)]
enum ConfigLoadError {
    Missing,
    Empty,
    Read(io::Error),
    Parse(serde_json::Error),
}

fn read_config_file(path: &std::path::Path) -> Result<Config, ConfigLoadError> {
    match fs::read_to_string(path) {
        Ok(content) if content.trim().is_empty() => Err(ConfigLoadError::Empty),
        Ok(content) => serde_json::from_str::<Config>(&content).map_err(ConfigLoadError::Parse),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Err(ConfigLoadError::Missing),
        Err(e) => Err(ConfigLoadError::Read(e)),
    }
}
```

- [ ] **Step 4: Run the recovery tests to verify they still fail only on missing fallback orchestration**

Run: `cargo test uses_backup_and_syncs_main -- --nocapture`
Expected: `FAIL` because `load_config_from_paths` still does not recover from backup.

- [ ] **Step 5: Implement the path-based fallback loader**

```rust
fn load_config_from_paths(main_path: &std::path::Path, backup_path: &std::path::Path) -> Config {
    match read_config_file(main_path) {
        Ok(config) => return normalize_loaded_config(config, Some(main_path)),
        Err(main_err) => {
            warn!("Primary config unavailable: {:?}", main_err);
        }
    }

    match read_config_file(backup_path) {
        Ok(config) => {
            info!("Recovered config from backup: {:?}", backup_path);
            let config = normalize_loaded_config(config, Some(main_path));
            if let Err(e) = save_config_to_path(main_path, &config) {
                error!("Failed to sync recovered config to main config: {}", e);
            }
            config
        }
        Err(backup_err) => {
            warn!("Backup config unavailable: {:?}", backup_err);
            Config::new()
        }
    }
}
```

- [ ] **Step 6: Refactor normalization and save logic to use explicit output paths**

```rust
fn normalize_loaded_config(config: Config, save_path: Option<&std::path::Path>) -> Config {
    deduplicate_exe_paths_with_target(config, save_path)
}

fn save_config_to_path(path: &std::path::Path, config: &Config) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_string_pretty(config)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    fs::write(path, content)
}
```

- [ ] **Step 7: Run the targeted tests to verify they pass**

Run: `cargo test config::tests -- --nocapture`
Expected: all config tests `PASS`

- [ ] **Step 8: Wire production `load_config()` to the path-based loader**

```rust
pub fn load_config() -> Config {
    ensure_config_dir().ok();

    let config_path = get_config_file_path();
    let backup_path = get_config_backup_file_path();
    info!("Loading config from: {:?}", config_path);
    info!("Backup config path: {:?}", backup_path);

    load_config_from_paths(&config_path, &backup_path)
}
```

- [ ] **Step 9: Run the focused test suite again**

Run: `cargo test config::tests -- --nocapture`
Expected: all config tests `PASS`

- [ ] **Step 10: Commit**

```bash
git add process-guard-service/src/config.rs process-guard-service/src/models.rs
git commit -m "feat: recover config from backup on startup"
```

### Task 4: Verify Package-Level Behavior

**Files:**
- Modify: `process-guard-service/src/config.rs`
- Modify: `process-guard-service/src/models.rs`
- Test: `process-guard-service/src/config.rs`

- [ ] **Step 1: Run the full crate test suite**

Run: `cargo test`
Expected: `PASS`

- [ ] **Step 2: Run a full compile check**

Run: `cargo check`
Expected: `Finished` without errors

- [ ] **Step 3: Inspect the diff for accidental backup-write paths**

Run: `git diff -- process-guard-service/src/models.rs process-guard-service/src/config.rs`
Expected: only reads from `config_bak.json`; all writes target `config.json`

- [ ] **Step 4: Commit the final verified state**

```bash
git add process-guard-service/src/models.rs process-guard-service/src/config.rs
git commit -m "test: verify config recovery fallback"
```
