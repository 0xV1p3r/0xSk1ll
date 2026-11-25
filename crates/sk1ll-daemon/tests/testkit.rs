//! Smoke test of `sk1ll_testkit::daemon` with the real binaries.
#![cfg(unix)]

use sk1ll_daemon::client::Connection;
use sk1ll_testkit::daemon::DaemonMachine;
use sk1ll_testkit::server::Server;

#[tokio::test(flavor = "multi_thread")]
async fn a_testkit_daemon_joins_a_testkit_server_and_reconnects_after_its_restart() {
    let mut server = Server::start().await;
    let mut machine = DaemonMachine::new("alpha", env!("CARGO_BIN_EXE_sk1ll-daemon"));
    machine.join(&server).await;
    assert_eq!(machine.state().machine_id, "alpha");
    machine.start_connected().await;
    server.stop();
    sk1ll_testkit::eventually(|| async { (machine.status().await.connection != Connection::Connected).then_some(()) })
        .await;
    server.restart().await;
    sk1ll_testkit::eventually(|| async { (machine.status().await.connection == Connection::Connected).then_some(()) })
        .await;
    assert!(server.row("alpha").await.is_some_and(|row| row.connected));
}
