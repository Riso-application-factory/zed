use std::path::Path;

#[test]
fn core_has_no_upstream_dependencies() -> std::io::Result<()> {
    for file in [
        "crew_quota.rs",
        "types.rs",
        "reducer.rs",
        "scheduler.rs",
        "normalize.rs",
    ] {
        let source =
            std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src").join(file))?;
        for dependency in [
            "gpui::",
            "project::",
            "workspace::",
            "db::",
            "http_client::",
            "credentials",
            "agent_servers::",
            "agent_client_protocol::",
            "acp_thread::",
            "fs::",
            "paths::",
        ] {
            assert!(!source.contains(dependency), "{file} imports {dependency}");
        }
    }
    Ok(())
}
