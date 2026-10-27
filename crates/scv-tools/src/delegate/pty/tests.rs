//! Unit tests for `src/delegate/pty.rs`.

use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use super::*;

/// A process on its own terminal of 80 by 24, with its output collected
/// as its I/O thread reads it.
struct Running {
    child: tokio::process::Child,
    pty: Arc<Pty>,
    seen: Arc<Mutex<String>>,
    io: Option<std::thread::JoinHandle<()>>,
}

/// Start `script` under `sh -c`.
fn sh(script: &str) -> Running {
    let spawned = spawn(&PtySpec {
        executable: Path::new("/bin/sh"),
        args: &["-c".to_owned(), script.to_owned()],
        cwd: Path::new("/"),
        environment: &[],
        cols: 80,
        rows: 24,
    })
    .unwrap();
    let pty = Arc::new(spawned.pty);
    let seen = Arc::new(Mutex::new(String::new()));
    let io = {
        let (pty, seen) = (Arc::clone(&pty), Arc::clone(&seen));
        std::thread::spawn(move || {
            let _ = pty.run_io(|output| {
                seen.lock()
                    .unwrap()
                    .push_str(&String::from_utf8_lossy(output));
                // Every `?` the process prints is asked a question of.
                if output.contains(&b'?') {
                    b"answer\r".to_vec()
                } else {
                    Vec::new()
                }
            });
        })
    };
    Running {
        child: spawned.child,
        pty,
        seen,
        io: Some(io),
    }
}

impl Running {
    async fn shows(&self, wanted: &str) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while !self.seen.lock().unwrap().contains(wanted) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("no {wanted:?} in {:?}", self.seen.lock().unwrap()));
    }

    /// Wait for the I/O thread to end.
    async fn io_ended(&mut self) {
        let io = self.io.take().unwrap();
        tokio::time::timeout(
            Duration::from_secs(10),
            tokio::task::spawn_blocking(move || io.join().unwrap()),
        )
        .await
        .expect("the terminal's I/O did not end")
        .unwrap();
    }

    async fn typed(&self, bytes: &[u8]) -> usize {
        self.pty
            .type_input(bytes, Instant::now() + Duration::from_secs(2))
            .await
    }
}

#[tokio::test]
async fn typed_input_reaches_the_process_and_its_output_comes_back() {
    let mut running = sh("echo ready; read line; echo \"got $line\"");
    running.shows("ready").await;
    assert_eq!(running.typed(b"hello\r").await, 6);
    running.shows("got hello").await;
    assert!(running.child.wait().await.unwrap().success());
    // Its side of the terminal is closed now, which ends the I/O.
    running.io_ended().await;
    assert_eq!(running.typed(b"late\r").await, 0);
}

#[tokio::test]
async fn a_reply_to_the_process_is_written_as_a_terminal_would_answer() {
    let running = sh("printf 'who? '; read line; echo \"said $line\"");
    running.shows("said answer").await;
}

#[tokio::test]
async fn the_process_leads_its_own_group_on_its_controlling_terminal_without_flow_control() {
    let running = sh(concat!(
        "[ \"$(ps -o pgid= -p $$ | tr -d ' ')\" = \"$$\" ] && echo leader-ok; ",
        "(exec 3</dev/tty) && echo controlling-ok; ",
        "stty -a | grep -q -- '-ixon' && echo flow-off; ",
        "stty size",
    ));
    running.shows("24 80").await;
    let seen = running.seen.lock().unwrap().clone();
    assert!(seen.contains("leader-ok"), "{seen}");
    assert!(seen.contains("controlling-ok"), "{seen}");
    assert!(seen.contains("flow-off"), "{seen}");
}

#[tokio::test]
async fn a_resize_reaches_the_process() {
    let running = sh("trap 'stty size; exit 0' WINCH; echo ready; while :; do sleep 0.05; done");
    running.shows("ready").await;
    running.pty.resize(50, 10).unwrap();
    running.shows("10 50").await;
}

#[tokio::test]
async fn closing_the_terminal_hangs_up_the_process() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("hangup");
    let mut running = sh(&format!(
        "trap 'echo hup > {}; exit 0' HUP; echo ready; while :; do sleep 0.05; done",
        marker.display()
    ));
    running.shows("ready").await;
    // What the owning process's death does: the master side goes.
    running.pty.stop();
    running.io_ended().await;
    let Ok(pty) = Arc::try_unwrap(running.pty) else {
        panic!("the terminal is still shared");
    };
    drop(pty);
    let status = tokio::time::timeout(Duration::from_secs(10), running.child.wait())
        .await
        .expect("the process outlived its terminal")
        .unwrap();
    assert!(status.success(), "{status:?}");
    assert_eq!(std::fs::read_to_string(&marker).unwrap().trim(), "hup");
}

#[tokio::test]
async fn input_a_process_does_not_read_is_dropped_at_the_deadline() {
    let running = sh("stty raw -echo; echo ready; sleep 30");
    running.shows("ready").await;
    let flood = vec![b'x'; 1024 * 1024];
    let started = std::time::Instant::now();
    let taken = running
        .pty
        .type_input(&flood, Instant::now() + Duration::from_millis(300))
        .await;
    assert!(taken < flood.len(), "the terminal took {taken} bytes");
    assert!(started.elapsed() < Duration::from_secs(5));
    // What it did not take is gone: the next keys go next.
    let idle = lock(&running.pty.io);
    assert!(idle.queue.is_empty(), "{:?}", idle.queue.len());
    assert_eq!(idle.offset, 0);
}

#[tokio::test]
async fn stopping_ends_the_io_even_while_the_process_runs() {
    let mut running = sh("echo ready; sleep 30");
    running.shows("ready").await;
    running.pty.stop();
    running.io_ended().await;
    assert_eq!(running.typed(b"x").await, 0);
    running.child.kill().await.unwrap();
}

#[tokio::test]
async fn none_of_the_terminals_own_descriptors_leak_into_the_process() {
    use std::os::fd::AsRawFd as _;
    // Linux creates every descriptor of the terminal closed on exec. The
    // process may hold others its test runner passed down (a jobserver's
    // pipes), so it is checked for the terminal's own: the master, the
    // wake pipe's two ends, and the slave anywhere but its standard
    // streams. Each is told by what it opens, not by its number.
    if !cfg!(target_os = "linux") {
        return;
    }
    let running = sh(
        r#"for fd in /proc/$$/fd/*; do printf '%s=%s ' "${fd##*/}" "$(readlink "$fd")"; done; echo end"#,
    );
    running.shows("end").await;
    let seen = running.seen.lock().unwrap().clone();
    let held: Vec<(&str, &str)> = seen
        .split_whitespace()
        .take_while(|word| *word != "end")
        .filter_map(|word| word.split_once('='))
        .collect();
    let opens = |fd: RawFd| {
        std::fs::read_link(format!("/proc/self/fd/{fd}"))
            .unwrap()
            .display()
            .to_string()
    };
    let own = [
        opens(running.pty.master.as_raw_fd()),
        opens(running.pty.wake_read.as_raw_fd()),
        opens(running.pty.wake_write.as_raw_fd()),
    ];
    assert!(own[0].ends_with("ptmx"), "{own:?}");
    assert!(
        own[1..].iter().all(|end| end.starts_with("pipe:")),
        "{own:?}"
    );
    let slave = held.iter().find(|(fd, _)| *fd == "0").map_or_else(
        || panic!("no standard input in {seen:?}"),
        |(_, opens)| *opens,
    );
    assert!(slave.starts_with("/dev/pts/"), "{seen:?}");
    for standard in ["0", "1", "2"] {
        assert!(held.contains(&(standard, slave)), "{seen:?}");
    }
    for (fd, opens) in &held {
        assert!(
            !own.iter().any(|own| own == opens),
            "fd {fd} is the terminal's own {opens}: {seen:?}"
        );
        if !["0", "1", "2"].contains(fd) {
            assert_ne!(*opens, slave, "fd {fd} is the slave again: {seen:?}");
        }
    }
}

#[tokio::test]
async fn input_waits_no_more_once_the_io_thread_fails() {
    let spawned = spawn(&PtySpec {
        executable: Path::new("/bin/sh"),
        args: &["-c".to_owned(), "echo hi; exec cat".to_owned()],
        cwd: Path::new("/"),
        environment: &[],
        cols: 80,
        rows: 24,
    })
    .unwrap();
    let mut child = spawned.child;
    let pty = Arc::new(spawned.pty);
    let io = {
        let pty = Arc::clone(&pty);
        std::thread::spawn(move || {
            let _ = pty.run_io(|_| panic!("a reader that fails"));
        })
    };
    let joined = tokio::task::spawn_blocking(move || io.join())
        .await
        .unwrap();
    assert!(joined.is_err(), "the reader should have failed");
    let started = tokio::time::Instant::now();
    let typed = pty
        .type_input(b"x", tokio::time::Instant::now() + Duration::from_secs(10))
        .await;
    assert_eq!(typed, 0);
    assert!(started.elapsed() < Duration::from_secs(1));
    child.kill().await.unwrap();
}
