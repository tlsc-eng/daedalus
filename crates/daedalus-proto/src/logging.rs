use std::fs::{self, OpenOptions};
use std::io::{self, IsTerminal};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::Mutex;

use tracing::level_filters::LevelFilter;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::Layer;
use tracing_subscriber::filter::{FilterExt, Targets};
use tracing_subscriber::fmt::writer::{BoxMakeWriter, MakeWriterExt};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// A log larger than this is moved to `<name>.log.1` at startup.
const MAX_LOG: u64 = 5 * 1024 * 1024;

/// Sends `tracing` events and `log` records to `path`, filtered by the `env` variable (default `info`).
pub fn init(path: &Path, env: &str) -> io::Result<()> {
    if fs::metadata(path).is_ok_and(|m| m.len() > MAX_LOG) {
        let _ = fs::rename(path, path.with_extension("log.1"));
    }
    let file = Mutex::new(
        OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(path)?,
    );
    // A spawned daemon's stderr already is its log file, so only tee to a real terminal.
    let writer = if cfg!(debug_assertions) && io::stderr().is_terminal() {
        BoxMakeWriter::new(file.and(io::stderr))
    } else {
        BoxMakeWriter::new(file)
    };
    let env_filter = EnvFilter::try_from_env(env).unwrap_or_else(|_| EnvFilter::new("info"));
    let layer = tracing_subscriber::fmt::layer()
        .with_writer(writer)
        .with_ansi(false)
        .with_filter(env_filter.and(driver_cap()));
    tracing_subscriber::registry()
        .with(layer)
        .try_init()
        .map_err(io::Error::other)
}

/// A separate filter that must also pass, so no DAEDALUS_LOG target (however specific) lets a driver log SQL or cell values.
fn driver_cap() -> Targets {
    Targets::new()
        .with_default(LevelFilter::TRACE)
        .with_target("tokio_postgres", LevelFilter::WARN)
        .with_target("mysql_async", LevelFilter::WARN)
        .with_target("rusqlite", LevelFilter::WARN)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing::Level;

    #[test]
    fn no_env_value_unmutes_the_db_drivers() {
        let cap = driver_cap();
        for target in [
            "tokio_postgres",
            "tokio_postgres::query",
            "mysql_async::conn",
            "rusqlite",
        ] {
            assert!(!cap.would_enable(target, &Level::DEBUG), "{target}");
            assert!(cap.would_enable(target, &Level::WARN), "{target}");
        }
        assert!(cap.would_enable("daedalus", &Level::TRACE));
    }

    #[test]
    fn oversized_log_is_rotated_and_reopened_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("daedalus-log-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("app.log");
        fs::write(&path, vec![b'x'; MAX_LOG as usize + 1]).unwrap();

        init(&path, "DAEDALUS_TEST_LOG").unwrap();
        tracing::info!("hello from the test");

        assert_eq!(
            fs::metadata(dir.join("app.log.1")).unwrap().len(),
            MAX_LOG + 1
        );
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("hello from the test"), "{text}");
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        fs::remove_dir_all(&dir).unwrap();
    }
}
