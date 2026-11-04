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
    let descendant = Command::new(std::env::current_exe()?)
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
