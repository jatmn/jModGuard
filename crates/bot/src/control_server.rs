//! Private writer listener. Started only after this process holds the lease.

use crate::state::BotState;
use jmodguard::control::{self, NonceWindow};
use jmodguard::db::{self, ControlReply};
use std::io::Write;
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const QUEUE: usize = 8;

struct Job {
    op: db::ControlOp,
    reply: std::sync::mpsc::Sender<ControlReply>,
}

pub fn spawn(app: Arc<BotState>) -> anyhow::Result<std::net::SocketAddr> {
    let bind = control::parse_private_bind(&app.cfg.bot.control_bind)?;
    let listener =
        TcpListener::bind(bind).map_err(|_| anyhow::anyhow!("control bind is not private"))?;
    let bound = listener
        .local_addr()
        .map_err(|_| anyhow::anyhow!("control bind is not private"))?;
    let key = control::load_writer_key(&app.cfg.db, true)?;
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Job>(QUEUE);
    let worker = Arc::clone(&app);
    tokio::spawn(async move {
        while let Some(job) = rx.recv().await {
            while worker.activation.moderation_depth() > 0 && worker.activation.may_record() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            let reply = if !worker.activation.may_record() {
                ControlReply::Waiting
            } else {
                match db::apply_control(&worker.db, Some(&worker.tenant), job.op).await {
                    Ok(reply) => reply,
                    Err(error) => ControlReply::Failed {
                        message: control::public_error(&error),
                    },
                }
            };
            let _ = job.reply.send(reply);
        }
    });
    let nonces = Arc::new(Mutex::new(NonceWindow::new()));
    let inflight = Arc::new(AtomicUsize::new(0));
    let activation = Arc::clone(&app.activation);
    std::thread::Builder::new()
        .name("writer-link".into())
        .spawn(move || accept_loop(listener, key, tx, nonces, inflight, activation))
        .map_err(|_| anyhow::anyhow!("control bind is not private"))?;
    Ok(bound)
}

fn accept_loop(
    listener: TcpListener,
    key: Vec<u8>,
    tx: tokio::sync::mpsc::Sender<Job>,
    nonces: Arc<Mutex<NonceWindow>>,
    inflight: Arc<AtomicUsize>,
    activation: Arc<crate::activation::Activation>,
) {
    loop {
        let Ok((stream, _)) = listener.accept() else {
            break;
        };
        if inflight.load(Ordering::Relaxed) >= 32 {
            let _ = reply(stream, &ControlReply::Busy);
            continue;
        }
        inflight.fetch_add(1, Ordering::Relaxed);
        let tx = tx.clone();
        let key = key.clone();
        let nonces = Arc::clone(&nonces);
        let inflight_conn = Arc::clone(&inflight);
        let activation = Arc::clone(&activation);
        if std::thread::Builder::new()
            .name("writer-link-conn".into())
            .spawn(move || {
                handle(stream, &key, &tx, &nonces, &activation);
                inflight_conn.fetch_sub(1, Ordering::Relaxed);
            })
            .is_err()
        {
            inflight.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

fn handle(
    mut stream: std::net::TcpStream,
    key: &[u8],
    tx: &tokio::sync::mpsc::Sender<Job>,
    nonces: &Mutex<NonceWindow>,
    activation: &crate::activation::Activation,
) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
    let body = match control::read_frame(&mut stream) {
        Ok(body) => body,
        Err(_) => return,
    };
    let op = match control::open_frame(key, &body, nonces) {
        Ok(op) => op,
        Err(error) => {
            let _ = reply(
                stream,
                &ControlReply::Failed {
                    message: control::public_error(&error),
                },
            );
            return;
        }
    };
    if !activation.may_record() {
        let _ = reply(stream, &ControlReply::Waiting);
        return;
    }
    let (send, recv) = std::sync::mpsc::channel();
    if tx.try_send(Job { op, reply: send }).is_err() {
        let _ = reply(stream, &ControlReply::Busy);
        return;
    }
    let done = match recv.recv_timeout(Duration::from_secs(30)) {
        Ok(done) => done,
        Err(_) => ControlReply::StillWaiting,
    };
    let _ = reply(stream, &done);
}

fn reply(mut stream: std::net::TcpStream, done: &ControlReply) -> anyhow::Result<()> {
    control::write_frame(&mut stream, done)?;
    stream.flush().ok();
    Ok(())
}
