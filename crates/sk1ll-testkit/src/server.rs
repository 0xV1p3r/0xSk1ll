//! A real sk1ll-server for end-to-end tests.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// The admin token every [`Server`] accepts.
pub const ADMIN: &str = "admin-secret";

type Tune = Arc<dyn Fn(&mut sk1ll_server::Config) + Send + Sync>;

/// A sk1ll-server on 127.0.0.1, serving on a runtime of its own so it can
/// be stopped (every connection closes, like a crashed server) and started
/// again on the same port with the same data. Stopped when dropped.
pub struct Server {
    port: u16,
    data: PathBuf,
    tune: Tune,
    _dir: tempfile::TempDir,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
    http: reqwest::Client,
}

impl std::fmt::Debug for Server {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Server").field("port", &self.port).field("data", &self.data).finish_non_exhaustive()
    }
}

impl Server {
    /// A fresh server with [`ADMIN`] as its admin token, started.
    pub async fn start() -> Self {
        Self::start_with(|_| {}).await
    }

    /// Like [`start`](Self::start), with `tune` applied to its config on
    /// every start.
    pub async fn start_with(tune: impl Fn(&mut sk1ll_server::Config) + Send + Sync + 'static) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let mut server = Self {
            port: 0,
            data,
            tune: Arc::new(tune),
            _dir: dir,
            stop: None,
            thread: None,
            http: reqwest::Client::new(),
        };
        server.restart().await;
        server
    }

    /// Starts it (again, if it ran) on the same port with the same data;
    /// returns once it answers `/healthz`.
    pub async fn restart(&mut self) {
        self.stop();
        let (port_tx, port_rx) = std::sync::mpsc::channel();
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let (data, port, tune) = (self.data.clone(), self.port, self.tune.clone());
        let thread = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
            rt.block_on(async move {
                let socket = tokio::net::TcpSocket::new_v4().unwrap();
                socket.set_reuseaddr(true).unwrap();
                socket.bind(SocketAddr::from(([127, 0, 0, 1], port))).unwrap();
                let listener = socket.listen(128).unwrap();
                port_tx.send(listener.local_addr().unwrap().port()).unwrap();
                let mut config = sk1ll_server::Config::new(data);
                config.admin_token = Some(ADMIN.into());
                tune(&mut config);
                tokio::select! {
                    r = sk1ll_server::serve(config, listener) => r.unwrap(),
                    _ = stop_rx => {}
                }
            });
            rt.shutdown_timeout(Duration::from_secs(2));
        });
        self.port = port_rx.recv().expect("the server binds");
        self.stop = Some(stop_tx);
        self.thread = Some(thread);
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

impl Drop for Server {
    fn drop(&mut self) {
        self.stop();
    }
}
