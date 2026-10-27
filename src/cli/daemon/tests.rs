//! Unit tests for `src/cli/daemon.rs`.

use std::sync::{Arc, Mutex};

use super::*;

/// A log written to memory.
#[derive(Clone, Default)]
struct Written(Arc<Mutex<Vec<u8>>>);

impl io::Write for Written {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().expect("the log").extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn a_taken_over_terminals_parser_never_logs_even_at_debug() {
    let written = Written::default();
    let writer = written.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(terminal_logs_off(tracing_subscriber::EnvFilter::new(
            "debug,vt100=debug",
        )))
        .with_writer(move || writer.clone())
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        tracing::debug!(target: "vt100::screen", "unhandled osc sequence: OSC 52;c;c2VjcmV0");
        tracing::debug!(target: "scv_server::workbench", "still logged");
    });
    let log = String::from_utf8(written.0.lock().expect("the log").clone()).expect("UTF-8");
    assert!(log.contains("still logged"), "{log}");
    assert!(!log.contains("OSC 52"), "{log}");
}
