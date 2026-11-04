//! Keeps upstream Zed types out of the catalog core, so upstream changes only reach the
//! adapter module (`src/upstream.rs`).

use std::path::Path;

const CORE_FILES: &[&str] = &["src/crew_model_catalog.rs", "src/extract.rs"];

const UPSTREAM_PATHS: &[&str] = &[
    "acp_thread",
    "agent_client_protocol",
    "agent_servers",
    "project::",
    "db::",
    "workspace::",
    "fs::",
    "paths::",
];

#[test]
fn core_does_not_reference_upstream_crates() {
    let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    for file in CORE_FILES {
        let source = std::fs::read_to_string(crate_dir.join(file))
            .unwrap_or_else(|error| panic!("reading {file}: {error}"));
        for upstream_path in UPSTREAM_PATHS {
            assert!(
                !source.contains(upstream_path),
                "{file} references `{upstream_path}`; keep upstream types in src/upstream.rs"
            );
        }
    }
}
