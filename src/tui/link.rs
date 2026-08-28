//! One task per configured VM. Local VMs hit the Unix socket directly, remote ones
//! ride `ssh host agents-hub stdio`; both erase to `AsyncRead + AsyncWrite` so `pump`
//! never learns which is which.

use super::Ui;
use crate::config::Vm;
use crate::proto::{Req, Resp};
use anyhow::{bail, Result};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

type Rd = Box<dyn AsyncRead + Unpin + Send>;
type Wr = Box<dyn AsyncWrite + Unpin + Send>;

async fn link(vm: &Vm) -> Result<(Rd, Wr, Option<tokio::process::Child>)> {
    match &vm.ssh {
        None => {
            let (r, w) = crate::connect_local().await?.into_split();
            Ok((Box::new(r), Box::new(w), None))
        }
        Some(host) => {
            let mut child = tokio::process::Command::new("ssh")
                // Never let a password prompt hang the TUI: keys/agent only.
                .args(["-o", "BatchMode=yes", "-o", "ServerAliveInterval=20"])
                .arg(host)
                .arg(&vm.remote_bin)
                .arg("stdio")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .spawn()?;
            let (Some(r), Some(w)) = (child.stdout.take(), child.stdin.take()) else {
                bail!("ssh pipes unavailable")
            };
            Ok((Box::new(r), Box::new(w), Some(child)))
        }
    }
}

async fn pump(
    idx: usize,
    r: Rd,
    mut w: Wr,
    tx: &UnboundedSender<Ui>,
    rx: &mut UnboundedReceiver<Req>,
) {
    let mut lines = BufReader::new(r).lines();
    loop {
        tokio::select! {
            line = lines.next_line() => match line {
                Ok(Some(l)) if !l.trim().is_empty() => {
                    if let Ok(resp) = serde_json::from_str::<Resp>(&l) {
                        if tx.send(Ui::Msg(idx, resp)).is_err() { return }
                    }
                }
                Ok(Some(_)) => {}
                _ => return,
            },
            req = rx.recv() => match req {
                Some(req) => {
                    let Ok(mut s) = serde_json::to_string(&req) else { continue };
                    s.push('\n');
                    if w.write_all(s.as_bytes()).await.is_err() { return }
                }
                None => return,
            },
        }
    }
}

/// Reconnects forever with capped backoff; a VM being down is a display state,
/// never a reason to exit.
pub async fn vm_task(idx: usize, vm: Vm, tx: UnboundedSender<Ui>, mut rx: UnboundedReceiver<Req>) {
    let mut backoff = 1u64;
    loop {
        if let Ok((r, w, child)) = link(&vm).await {
            backoff = 1;
            if tx.send(Ui::Up(idx)).is_err() {
                return;
            }
            pump(idx, r, w, &tx, &mut rx).await;
            drop(child); // kill_on_drop reaps ssh
        }
        if tx.send(Ui::Down(idx)).is_err() {
            return;
        }
        tokio::time::sleep(Duration::from_secs(backoff)).await;
        backoff = (backoff * 2).min(30);
    }
}
