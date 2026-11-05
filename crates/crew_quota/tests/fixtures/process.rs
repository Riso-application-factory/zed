use async_process::Command;
use std::{
    io::{self, BufRead as _, Write as _},
    path::PathBuf,
    time::Duration,
};

fn main() -> io::Result<()> {
    let directory = PathBuf::from(
        std::env::var_os("CREW_QUOTA_FIXTURE_DIRECTORY")
            .ok_or_else(|| io::Error::other("fixture directory missing"))?,
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    if std::env::args().any(|argument| argument == "--descendant") {
        loop {
            if std::time::Instant::now() >= deadline {
                return Ok(());
            }
            std::fs::write(
                directory.join("heartbeat"),
                format!("{:?}", std::time::SystemTime::now()),
            )?;
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    let mut descendant = Command::new(std::env::current_exe()?)
        .arg("--descendant")
        .spawn()?;
    std::fs::write(
        directory.join("launcher.pid"),
        std::process::id().to_string(),
    )?;
    std::fs::write(
        directory.join("descendant.pid"),
        descendant.id().to_string(),
    )?;
    let mode = std::env::var("CREW_QUOTA_FIXTURE_MODE")
        .map_err(|_| io::Error::other("fixture mode missing"))?;
    if mode.starts_with("agy-") {
        return antigravity(&directory, &mode, &mut descendant);
    }
    let stdin = io::stdin();
    let mut lines = stdin.lock().lines();
    if lines.next().transpose()?.is_none() {
        return Ok(());
    }
    println!("{{\"id\":1,\"result\":{{}}}}");
    io::stdout().flush()?;
    if lines.next().transpose()?.is_none() || lines.next().transpose()?.is_none() {
        return Ok(());
    }
    match mode.as_str() {
        "success" => println!("{{\"id\":2,\"result\":{{}}}}"),
        "protocol" => println!("invalid protocol"),
        "timeout" | "cancel" => loop {
            if std::time::Instant::now() >= deadline {
                return Ok(());
            }
            std::thread::sleep(Duration::from_secs(1));
        },
        _ => return Err(io::Error::other("invalid fixture mode")),
    }
    io::stdout().flush()?;
    for line in lines {
        line?;
    }
    Ok(())
}

fn antigravity(
    directory: &std::path::Path,
    mode: &str,
    descendant: &mut async_process::Child,
) -> io::Result<()> {
    use serde_json::{Value, json};
    let session = "fixture-owned-session";
    for line in io::stdin().lock().lines() {
        let request: Value = serde_json::from_str(&line?).map_err(io::Error::other)?;
        let identifier = request
            .get("id")
            .cloned()
            .ok_or_else(|| io::Error::other("id missing"))?;
        let method = request
            .get("method")
            .and_then(Value::as_str)
            .ok_or_else(|| io::Error::other("method missing"))?;
        let result = match method {
            "initialize" => json!({"protocolVersion":1,"agentCapabilities":{}}),
            "session/new" => {
                for suffix in [".db", ".db-shm", ".db-wal", ".meta"] {
                    std::fs::write(
                        directory
                            .join("conversations")
                            .join(format!("{session}{suffix}")),
                        "fixture",
                    )?;
                }
                println!(
                    "{}",
                    json!({"jsonrpc":"2.0", "method":"session/update", "params":{"sessionId":session,"update":{"sessionUpdate":"available_commands_update", "availableCommands":[{"name":"usage"}]}}})
                );
                json!({"sessionId":session,"_meta":{"antigravity":{"tier":{"name":"Fixture tier"}}}})
            }
            "session/prompt" => {
                if mode == "agy-timeout" {
                    std::thread::sleep(Duration::from_secs(60));
                }
                let update = if mode == "agy-tool" {
                    json!({"sessionUpdate":"tool_call"})
                } else {
                    json!({"sessionUpdate":"agent_message_chunk", "content":{"type":"text","text":include_str!("antigravity.sanitized.md")}})
                };
                println!(
                    "{}",
                    json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":session,"update":update}})
                );
                json!({"stopReason":"end_turn"})
            }
            "session/close" => {
                // Reap the fixture child before its parent is killed, so group-exit checks do not depend on launchd reaping an orphan.
                descendant.kill()?;
                smol::block_on(descendant.status())?;
                std::fs::write(directory.join("closed"), session)?;
                json!({})
            }
            _ => return Err(io::Error::other("unexpected method")),
        };
        println!(
            "{}",
            json!({"jsonrpc":"2.0","id":identifier,"result":result})
        );
        io::stdout().flush()?;
    }
    Ok(())
}
