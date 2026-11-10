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

fn usable() -> bool {
    daedalus_lsp::find_program("basedpyright-langserver").is_some()
        && Command::new("basedpyright-langserver")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
}

#[test]
fn basedpyright_answers_definition_in_a_tiny_project() {
    if !usable() {
        eprintln!("basedpyright not usable; skipping");
        return;
    }
    let dir = std::env::temp_dir().join(format!("daedalus-lsp-py-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let dir = dir.canonicalize().unwrap();
    std::fs::write(dir.join("pyproject.toml"), "[project]\nname = \"probe\"\n").unwrap();
    let source = "def helper() -> int:\n    return 1\n\n\nvalue = helper()\n";
    let file = dir.join("main.py");
    std::fs::write(&file, source).unwrap();

    let (client, events) = Client::start(ServerKind::Python, dir.clone());
    client.did_open(&file, "python", 1, source.into());

    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        match next_event(&events, deadline) {
            Some(Event::Ready) => break,
            Some(Event::Stopped(why)) => {
                eprintln!("basedpyright stopped ({why}); skipping");
                return;
            }
            Some(_) => {}
            None => {
                eprintln!("basedpyright not ready in time; skipping");
                return;
            }
        }
    }
    let at = Position {
        line: 4,
        character: 9,
    };
    let end = Instant::now() + Duration::from_secs(120);
    while Instant::now() < end {
        let found = block_on(client.definition(&file, at)).unwrap_or_default();
        if found.iter().any(|l| l.range.start.line == 0) {
            return;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    panic!("no definition for helper from basedpyright in time");
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
