//! A real sk1ll-server for end-to-end tests.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// The admin token every [`Server`] accepts.
pub const ADMIN: &str = "admin-secret";

type Tune = Arc<dyn Fn(&mut sk1ll_server::Config) + Send + Sync>;
type Socket = Arc<dyn Fn(&tokio::net::TcpSocket) + Send + Sync>;

/// A sk1ll-server on 127.0.0.1, serving on a runtime of its own so it can
/// be stopped (every connection closes, like a crashed server) and started
/// again on the same port with the same data. Stopped when dropped.
pub struct Server {
    port: u16,
    data: PathBuf,
    start: Builder,
    _dir: tempfile::TempDir,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
    task: Option<tokio::task::JoinHandle<()>>,
    http: reqwest::Client,
}

impl std::fmt::Debug for Server {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Server").field("port", &self.port).field("data", &self.data).finish_non_exhaustive()
    }
}

/// How a [`Server`] starts, for servers that need more than a config tweak.
#[derive(Clone)]
pub struct Builder {
    tune: Tune,
    socket: Socket,
    here: bool,
}

impl Default for Builder {
    fn default() -> Self {
        Self { tune: Arc::new(|_| {}), socket: Arc::new(|_| {}), here: false }
    }
}

impl Builder {
    /// Applies `tune` to its config on every start, after earlier tunes.
    pub fn config(mut self, tune: impl Fn(&mut sk1ll_server::Config) + Send + Sync + 'static) -> Self {
        let before = self.tune;
        self.tune = Arc::new(move |config| {
            before(config);
            tune(config);
        });
        self
    }

    /// Applies `f` to the listening socket before it binds, e.g. a small
    /// send buffer, which accepted connections inherit.
    pub fn socket(mut self, f: impl Fn(&tokio::net::TcpSocket) + Send + Sync + 'static) -> Self {
        self.socket = Arc::new(f);
        self
    }

    /// Serves on the caller's runtime instead of one of its own, so the
    /// server shares the test's clock (`tokio::time::pause`) and workers.
    /// Such a server can't [`restart`](Server::restart), and its
    /// connections outlive [`stop`](Server::stop).
    pub fn on_caller_runtime(mut self) -> Self {
        self.here = true;
        self
    }

    /// Starts it; returns once it answers `/healthz`.
    pub async fn start(self) -> Server {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let mut server = Server {
            port: 0,
            data,
            start: self,
            _dir: dir,
            stop: None,
            thread: None,
            task: None,
            http: reqwest::Client::new(),
        };
        server.restart().await;
        server
    }
}

impl Server {
    /// A fresh server with [`ADMIN`] as its admin token, started.
    pub async fn start() -> Self {
        Self::builder().start().await
    }

    /// Like [`start`](Self::start), with `tune` applied to its config on
    /// every start.
    pub async fn start_with(tune: impl Fn(&mut sk1ll_server::Config) + Send + Sync + 'static) -> Self {
        Self::builder().config(tune).start().await
    }

    /// A server to configure before [`start`](Builder::start).
    pub fn builder() -> Builder {
        Builder::default()
    }

    /// Starts it (again, if it ran) on the same port with the same data;
    /// returns once it answers `/healthz`.
    pub async fn restart(&mut self) {
        assert!(self.task.is_none(), "a server on the caller's runtime can't restart");
        self.stop();
        let (port_tx, port_rx) = tokio::sync::oneshot::channel();
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let (data, port, start) = (self.data.clone(), self.port, self.start.clone());
        let here = start.here;
        let serve = async move {
            let listener = listen(&start.socket, port).await;
            port_tx.send(listener.local_addr().unwrap().port()).unwrap();
            let mut config = sk1ll_server::Config::new(data);
            config.admin_token = Some(ADMIN.into());
            (start.tune)(&mut config);
            tokio::select! {
                r = sk1ll_server::serve(config, listener) => r.unwrap(),
                _ = stop_rx => {}
            }
        };
        if here {
            self.task = Some(tokio::spawn(serve));
        } else {
            self.thread = Some(std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
                rt.block_on(serve);
                rt.shutdown_timeout(Duration::from_secs(2));
            }));
        }
        self.port = port_rx.await.expect("the server binds");
        self.stop = Some(stop_tx);
        let health = format!("{}/healthz", self.url());
        crate::eventually(|| async { self.http.get(&health).send().await.ok().map(drop) }).await;
    }

    /// Stops it, closing every connection. [`restart`](Self::restart)
    /// starts it again.
    pub fn stop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
        if let Some(task) = &self.task {
            task.abort();
        }
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// `http://127.0.0.1:<port>`.
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// Its data directory.
    pub fn data(&self) -> &Path {
        &self.data
    }

    /// An HTTP client for it.
    pub fn http(&self) -> &reqwest::Client {
        &self.http
    }

    /// `GET /api<path>` as admin, which must succeed; its JSON body.
    pub async fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> T {
        let resp = self.http.get(format!("{}/api{path}", self.url())).bearer_auth(ADMIN).send().await.unwrap();
        assert!(resp.status().is_success(), "GET {path}: {}", resp.status());
        resp.json().await.unwrap()
    }

    /// A join code of `scope`, made by the admin.
    pub async fn join_code_for(&self, scope: sk1ll_proto::Scope) -> sk1ll_proto::JoinCode {
        let url = format!("{}/api/join-codes", self.url());
        let body = sk1ll_proto::JoinCodeRequest { scope };
        let resp = self.http.post(url).bearer_auth(ADMIN).json(&body).send().await.unwrap();
        assert!(resp.status().is_success(), "POST /join-codes: {}", resp.status());
        resp.json().await.unwrap()
    }

    /// A join code for a Machine that may manage the whole fleet.
    pub async fn join_code(&self) -> String {
        self.join_code_for(sk1ll_proto::Scope::default()).await.code
    }

    /// The fleet's row for Machine `id`, if it has one.
    pub async fn row(&self, id: &str) -> Option<sk1ll_proto::MachineRow> {
        self.get::<sk1ll_proto::FleetView>("/fleet").await.machines.into_iter().find(|m| m.id == id)
    }

    /// The bare repo's `HEAD`, read directly; `None` before the first commit.
    pub fn head(&self) -> Option<String> {
        let mut cmd = std::process::Command::new("git");
        cmd.arg("--git-dir").arg(self.data.join("repo.git")).args(["rev-parse", "--verify", "--quiet", "HEAD"]);
        let out = crate::git::isolate(&mut cmd).output().unwrap();
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    }
}

/// Listens on `port` (`0`: any free one). A restart may find the port still
/// taken by the old server's connections while they close, so it retries.
async fn listen(setup: &Socket, port: u16) -> tokio::net::TcpListener {
    let bind = || {
        let socket = tokio::net::TcpSocket::new_v4()?;
        socket.set_reuseaddr(true)?;
        setup(&socket);
        socket.bind(SocketAddr::from(([127, 0, 0, 1], port)))?;
        socket.listen(128)
    };
    for _ in 0..200 {
        match bind() {
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse && port != 0 => {}
            listener => return listener.unwrap(),
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("port {port} stayed in use");
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop();
    }
}
