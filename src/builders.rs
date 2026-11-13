//! Builders that connect to the server (`agentflow-builder`, the owner, 2026-11-13): instead of
//! the server reaching a build machine over SSH, the machine connects out over a WebSocket and the
//! server sends its requests that way. Nothing on the builder accepts connections, and it can sit
//! behind any NAT; it's online exactly while it's connected.
//!
//! The requests are the ones the SSH path sends (`crate::remote`): a shell script whose output
//! comes back whole, or streamed line by line (a job); plus syncing a working tree from the
//! server's content store and sending a job's results back. A job keeps running if the connection
//! drops: the builder keeps its output and sends it when it's back.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};

/// What the server asks a builder to do.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// Run a script with `sh` in the builder's home; `stream`: send its lines as they come.
    Sh { id: u64, script: String, stream: bool, timeout_s: u64 },
    /// Bring a project's work folder to a snapshot in the server's store.
    Sync { id: u64, work: String, manifest: String, bundle: String, commit: String },
    /// Send a path of a finished job back to the server.
    Upload { id: u64, job: i64, from: String, name: String },
    /// Stop a streamed script.
    Kill { id: u64 },
}

impl Request {
    pub fn id(&self) -> u64 {
        match self {
            Request::Sh { id, .. } | Request::Sync { id, .. } | Request::Upload { id, .. } | Request::Kill { id } => {
                *id
            }
        }
    }
}

/// What a builder says.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Reply {
    /// Right after connecting: who it is.
    Hello { name: String, version: String, threads: u64, memory_gb: f64 },
    /// A line of a streamed script (`err`: from stderr).
    Line { re: u64, text: String, err: bool },
    /// A request is done.
    Done { re: u64, code: i32, stdout: String, stderr: String },
    /// A request couldn't be done.
    Failed { re: u64, error: String },
}

/// What happens to a streamed script, for the job that waits on it.
#[derive(Debug)]
pub enum Event {
    Line(String),
    Exit(i32),
    Error(String),
}

enum Waiter {
    Whole(oneshot::Sender<anyhow::Result<(i32, String, String)>>),
    Stream(mpsc::UnboundedSender<Event>),
}

#[derive(Default)]
struct Inner {
    /// Connected builders: where their requests go.
    online: HashMap<String, (u64, mpsc::UnboundedSender<Request>)>,
    /// Requests waiting for an answer, by id; they outlive a connection (a job runs on).
    waiting: HashMap<u64, (String, Waiter)>,
    next: u64,
    hello: HashMap<String, (String, u64, f64)>,
}

/// The connected builders.
#[derive(Default, Clone)]
pub struct Hub(Arc<Mutex<Inner>>);

static HUB: OnceLock<Hub> = OnceLock::new();

/// The server's hub (one per process).
pub fn hub() -> &'static Hub {
    HUB.get_or_init(Hub::default)
}

impl Hub {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn is_online(&self, name: &str) -> bool {
        self.lock().online.contains_key(name)
    }

    /// Version, threads and memory a builder reported.
    pub fn hello(&self, name: &str) -> Option<(String, u64, f64)> {
        self.lock().hello.get(name).cloned()
    }

    /// A builder connected; requests for it go to `tx` from now on. Returns the connection's
    /// number, so an older connection that ends later doesn't unregister this one.
    pub fn connect(&self, name: &str, tx: mpsc::UnboundedSender<Request>) -> u64 {
        let mut g = self.lock();
        g.next += 1;
        let n = g.next;
        g.online.insert(name.to_owned(), (n, tx));
        n
    }

    pub fn disconnect(&self, name: &str, conn: u64) {
        let mut g = self.lock();
        if g.online.get(name).is_some_and(|(n, _)| *n == conn) {
            g.online.remove(name);
        }
    }

    /// What a builder said.
    pub fn receive(&self, name: &str, reply: Reply) {
        let mut g = self.lock();
        match reply {
            Reply::Hello { version, threads, memory_gb, .. } => {
                g.hello.insert(name.to_owned(), (version, threads, memory_gb));
            }
            Reply::Line { re, text, err: _ } => {
                if let Some((owner, Waiter::Stream(tx))) = g.waiting.get(&re)
                    && owner == name
                {
                    let _ = tx.send(Event::Line(text));
                }
            }
            Reply::Done { re, code, stdout, stderr } => {
                if g.waiting.get(&re).is_some_and(|(o, _)| o == name)
                    && let Some((_, w)) = g.waiting.remove(&re)
                {
                    match w {
                        Waiter::Whole(tx) => {
                            let _ = tx.send(Ok((code, stdout, stderr)));
                        }
                        Waiter::Stream(tx) => {
                            let _ = tx.send(Event::Exit(code));
                        }
                    }
                }
            }
            Reply::Failed { re, error } => {
                if g.waiting.get(&re).is_some_and(|(o, _)| o == name)
                    && let Some((_, w)) = g.waiting.remove(&re)
                {
                    match w {
                        Waiter::Whole(tx) => {
                            let _ = tx.send(Err(anyhow::anyhow!("{error}")));
                        }
                        Waiter::Stream(tx) => {
                            let _ = tx.send(Event::Error(error));
                        }
                    }
                }
            }
        }
    }

    fn send(&self, name: &str, make: impl FnOnce(u64) -> Request, waiter: Waiter) -> anyhow::Result<u64> {
        let mut g = self.lock();
        let tx = g.online.get(name).map(|(_, tx)| tx.clone()).with_context(|| format!("{name} isn't connected"))?;
        g.next += 1;
        let id = g.next;
        g.waiting.insert(id, (name.to_owned(), waiter));
        if tx.send(make(id)).is_err() {
            g.waiting.remove(&id);
            bail!("{name} just disconnected");
        }
        Ok(id)
    }

    /// Asks and waits for the whole answer: (exit code, stdout, stderr).
    pub async fn ask(
        &self,
        name: &str,
        make: impl FnOnce(u64) -> Request,
        timeout: Duration,
    ) -> anyhow::Result<(i32, String, String)> {
        let (tx, rx) = oneshot::channel();
        let id = self.send(name, make, Waiter::Whole(tx))?;
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => bail!("{name} dropped the request"),
            Err(_) => {
                self.lock().waiting.remove(&id);
                bail!("{name} didn't answer within {} s", timeout.as_secs())
            }
        }
    }

    /// Starts a streamed script; its lines and its end come on the receiver.
    pub fn stream(&self, name: &str, script: String) -> anyhow::Result<(u64, mpsc::UnboundedReceiver<Event>)> {
        let (tx, rx) = mpsc::unbounded_channel();
        let id = self.send(name, |id| Request::Sh { id, script, stream: true, timeout_s: 0 }, Waiter::Stream(tx))?;
        Ok((id, rx))
    }

    pub fn kill(&self, name: &str, id: u64) {
        let g = self.lock();
        if let Some((_, tx)) = g.online.get(name) {
            let _ = tx.send(Request::Kill { id });
        }
    }

    /// Closes a builder's connection (it was renamed or removed); it reconnects as what its token
    /// says now, or not at all.
    pub fn drop_builder(&self, name: &str) {
        self.lock().online.remove(name);
    }

    /// Forgets a request nobody waits for any more.
    pub fn forget(&self, id: u64) {
        self.lock().waiting.remove(&id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn answers_reach_their_request_and_only_from_its_builder() {
        let hub = Hub::default();
        let (tx, mut rx) = mpsc::unbounded_channel();
        hub.connect("hq", tx);
        let h = hub.clone();
        let ask = tokio::spawn(async move {
            h.ask(
                "hq",
                |id| Request::Sh { id, script: "echo hi".into(), stream: false, timeout_s: 5 },
                Duration::from_secs(5),
            )
            .await
        });
        let Some(Request::Sh { id, .. }) = rx.recv().await else { panic!("no request") };
        // Another builder can't answer it.
        hub.receive("other", Reply::Done { re: id, code: 9, stdout: "forged".into(), stderr: String::new() });
        hub.receive("hq", Reply::Done { re: id, code: 0, stdout: "hi\n".into(), stderr: String::new() });
        assert_eq!(ask.await.unwrap().unwrap(), (0, "hi\n".into(), String::new()));

        let (id, mut events) = hub.stream("hq", "make".into()).unwrap();
        let _ = rx.recv().await;
        // The connection drops and comes back: the job's output still arrives.
        let (tx2, _rx2) = mpsc::unbounded_channel();
        hub.connect("hq", tx2);
        hub.receive("hq", Reply::Line { re: id, text: "building".into(), err: false });
        hub.receive("hq", Reply::Done { re: id, code: 0, stdout: String::new(), stderr: String::new() });
        assert!(matches!(events.recv().await, Some(Event::Line(l)) if l == "building"));
        assert!(matches!(events.recv().await, Some(Event::Exit(0))));
        assert!(hub.ask("nobody", |id| Request::Kill { id }, Duration::from_secs(1)).await.is_err());
    }
}
