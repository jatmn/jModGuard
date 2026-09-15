//! Local-process stand-in for bot/API/conductor health during update tests.
//! Production binaries are not this helper; it only answers `/health` and drains.
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

static DRAINING: AtomicBool = AtomicBool::new(false);

fn main() {
    let port: u16 = std::env::var("JMODGUARD_COMPONENT_PORT")
        .expect("JMODGUARD_COMPONENT_PORT")
        .parse()
        .expect("port");
    let generation = std::env::var("JMODGUARD_OWNER_GENERATION").expect("generation");
    let owner_file = std::env::var("JMODGUARD_OWNER_FILE").expect("owner file");
    let fail_ready = std::env::var("JMODGUARD_FAIL_READY").as_deref() == Ok("1");
    let hold_drain = std::env::var("JMODGUARD_HOLD_DRAIN").as_deref() == Ok("1");

    #[cfg(unix)]
    install_term(hold_drain);

    let listener = TcpListener::bind(("127.0.0.1", port)).expect("bind component health");
    listener
        .set_nonblocking(true)
        .expect("nonblocking health listener");
    loop {
        if DRAINING.load(Ordering::SeqCst) && !hold_drain {
            break;
        }
        match listener.accept() {
            Ok((mut stream, _)) => {
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf);
                let current = std::fs::read_to_string(&owner_file).unwrap_or_default();
                let owner_ok = current.trim() == generation;
                let ready = !fail_ready && owner_ok && !DRAINING.load(Ordering::SeqCst);
                let (status, body) = if ready {
                    ("200 OK", "ok")
                } else {
                    ("503 Service Unavailable", "not-ready")
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(_) => break,
        }
    }
}

#[cfg(unix)]
fn install_term(hold_drain: bool) {
    unsafe {
        let handler = if hold_drain {
            ignore_term as *const () as usize
        } else {
            handle_term as *const () as usize
        };
        libc::signal(libc::SIGTERM, handler);
    }
}

#[cfg(unix)]
extern "C" fn handle_term(_: i32) {
    DRAINING.store(true, Ordering::SeqCst);
}

#[cfg(unix)]
extern "C" fn ignore_term(_: i32) {}
