//! Helpers shared by the black-box tests: instance isolation, private
//! files, a scripted Responses provider, and process inspection.

use std::{io::BufRead, os::unix::fs::PermissionsExt as _, path::Path};

use serde_json::{Value, json};

/// SCV selectors a developer's shell may export, which would otherwise leak a
/// real configuration or delegation context into a spawned binary.
const INHERITED_SCV_ENV: &[&str] = &[
    "SCV_CONFIG",
    "SCV_PARENT",
    "SCV_DELEGATION_DEPTH",
    "SCV_MODEL",
    "SCV_PROVIDER",
    "SCV_BASE_URL",
    "SCV_API_KEY_ENV",
];

/// Confines a spawned SCV binary to a temporary instance home.
///
/// `SCV_HOME` selects the instance and `HOME` points inside it, so neither an
/// explicit nor a fallback lookup can reach the developer's real `~/.scv`.
/// Every test that spawns an SCV binary must use this; the
/// `every_spawned_scv_binary_is_isolated` guard enforces it.
pub(crate) trait Isolated {
    fn isolated(&mut self, home: &Path) -> &mut Self;
}

impl Isolated for std::process::Command {
    fn isolated(&mut self, home: &Path) -> &mut Self {
        if let Some(real) = std::env::var_os("HOME") {
            assert!(
                !home.starts_with(Path::new(&real).join(".scv")),
                "test home {} is inside the real ~/.scv",
                home.display()
            );
        }
        let user_home = home.join(".no-user-home");
        self.env("SCV_HOME", home).env("HOME", &user_home);
        for variable in INHERITED_SCV_ENV {
            self.env_remove(variable);
        }
        self
    }
}

impl Isolated for tokio::process::Command {
    fn isolated(&mut self, home: &Path) -> &mut Self {
        self.as_std_mut().isolated(home);
        self
    }
}

/// Write `contents` to `path`, creating its directory, with mode 0600.
pub(crate) fn write_private(path: &Path, contents: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
}

/// A Responses event stream in which the model calls tool `name` once, as
/// call `call_id`.
pub(crate) fn call(call_id: &str, name: &str, arguments: Value) -> String {
    let delta = json!({"type":"response.function_call_arguments.delta","output_index":0,"delta":arguments.to_string()});
    let done = json!({"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","call_id":call_id,"name":name}});
    format!(
        "data: {delta}\n\ndata: {done}\n\ndata: {{\"type\":\"response.completed\",\"response\":{{}}}}\n\n"
    )
}

/// A Responses event stream in which the model answers `content`.
pub(crate) fn text(content: &str) -> String {
    let delta = json!({"type":"response.output_text.delta","delta":content});
    format!("data: {delta}\n\ndata: {{\"type\":\"response.completed\",\"response\":{{}}}}\n\n")
}

/// Read one HTTP request from `reader` and return its body, sized by its
/// `Content-Length`.
pub(crate) fn read_http_request(reader: &mut impl BufRead) -> Vec<u8> {
    let mut length = 0;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        if line == "\r\n" || line.is_empty() {
            break;
        }
        if let Some((key, value)) = line.split_once(':')
            && key.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse().unwrap();
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    body
}

/// An HTTP 200 response carrying the event stream `body`.
pub(crate) fn sse_response(body: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// Processes whose `SCV_PARENT` chain names delegation `handle`. Reads
/// `/proc`, so it finds nothing where there is none.
pub(crate) fn tagged(handle: &str) -> Vec<u32> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let pid: u32 = entry.file_name().to_str()?.parse().ok()?;
            let environ = std::fs::read(entry.path().join("environ")).ok()?;
            environ
                .split(|byte| *byte == 0)
                .filter_map(|entry| entry.strip_prefix(b"SCV_PARENT="))
                .any(|chain| {
                    String::from_utf8_lossy(chain)
                        .split(';')
                        .any(|entry| entry.rsplit('/').next() == Some(handle))
                })
                .then_some(pid)
        })
        .collect()
}

/// Whether process `pid` is running; a zombie has exited and does not count.
pub(crate) fn alive(pid: u32) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        // No procfs (macOS): ask the kernel.
        // SAFETY: signal 0 only checks that the process exists.
        return unsafe { libc::kill(pid as i32, 0) } == 0;
    };
    // A zombie has exited; only its parent's wait is missing.
    !stat
        .rsplit(')')
        .next()
        .is_some_and(|rest| rest.trim_start().starts_with('Z'))
}
