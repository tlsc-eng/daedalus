use std::process::Command;
use std::time::{Duration, Instant};

use daedalus_lsp::{Client, Event, Position, ServerKind};

fn next_event(events: &async_channel::Receiver<Event>, deadline: Instant) -> Option<Event> {
    while Instant::now() < deadline {
        if let Ok(event) = events.try_recv() {
            return Some(event);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    None
}

/// The rustup proxy exists on PATH even without the component, and then exits non-zero.
fn usable() -> bool {
    daedalus_lsp::find_program("rust-analyzer").is_some_and(|program| {
        Command::new(program)
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    })
}

#[test]
fn rust_analyzer_answers_definition_in_a_tiny_crate() {
    if !usable() {
        eprintln!("rust-analyzer not usable; skipping");
        return;
    }
    let dir = std::env::temp_dir().join(format!("daedalus-lsp-rust-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("src")).unwrap();
    let dir = dir.canonicalize().unwrap();
    std::fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"probe\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    let source = "fn helper() -> i32 {\n    1\n}\n\nfn main() {\n    let _ = helper();\n}\n";
    let file = dir.join("src/main.rs");
    std::fs::write(&file, source).unwrap();

    let (client, events) = Client::start(ServerKind::Rust, dir.clone());
    client.did_open(&file, "rust", 1, source.into());

    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        match next_event(&events, deadline) {
            Some(Event::Ready) => break,
            Some(Event::Stopped(why)) => panic!("rust-analyzer stopped: {why}"),
            Some(_) => {}
            None => panic!("rust-analyzer not ready in time"),
        }
    }
    let at = Position {
        line: 5,
        character: 14,
    };
    let end = Instant::now() + Duration::from_secs(120);
    while Instant::now() < end {
        let found = block_on(client.definition(&file, at)).unwrap_or_default();
        if found.iter().any(|l| l.range.start.line == 0) {
            return;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    panic!("no definition for helper from rust-analyzer in time");
}

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    use std::pin::pin;
    use std::task::{Context, Poll, Wake, Waker};
    struct Thread(std::thread::Thread);
    impl Wake for Thread {
        fn wake(self: std::sync::Arc<Self>) {
            self.0.unpark();
        }
    }
    let waker = Waker::from(std::sync::Arc::new(Thread(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    let mut future = pin!(future);
    loop {
        if let Poll::Ready(out) = future.as_mut().poll(&mut cx) {
            return out;
        }
        std::thread::park_timeout(Duration::from_millis(100));
    }
}
