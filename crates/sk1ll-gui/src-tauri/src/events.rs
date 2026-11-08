//! One SSE subscription; only connection attempts are retried, never commands.
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;
use sk1ll_daemon::client::Client;
use tokio::time::{Instant, sleep, timeout};

const MIN_DELAY: Duration = Duration::from_millis(500);
const MAX_DELAY: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Serialize)]
struct ConnectionEvent {
    reachable: bool,
    message: Option<String>,
    retry_in_ms: u64,
}

#[derive(Default)]
struct Backoff(u32);

impl Backoff {
    fn next(&mut self) -> Duration {
        let delay = (MIN_DELAY * 2u32.pow(self.0.min(6))).min(MAX_DELAY);
        self.0 = self.0.saturating_add(1);
        delay
    }

    fn restore(&mut self) {
        self.0 = 0;
    }
}

pub async fn forward(client: Client, emit: impl Fn(&str, Value) + Send + 'static) {
    let mut backoff = Backoff::default();
    loop {
        let started = Instant::now();
        let error = match timeout(Duration::from_secs(5), client.events()).await {
            Ok(Ok(mut stream)) => {
                emit_connection(&emit, true, None, Duration::ZERO);
                let message = loop {
                    match stream.next().await {
                        Some(Ok(event)) => {
                            backoff.restore();
                            emit(event.name(), serde_json::to_value(&event).expect("serializing an event"));
                        }
                        Some(Err(error)) => break error.to_string(),
                        None => break "Daemon closed its event stream".into(),
                    }
                };
                if started.elapsed() >= MAX_DELAY {
                    backoff.restore();
                }
                message
            }
            Ok(Err(error)) => error.to_string(),
            Err(_) => "Daemon event subscription timed out".into(),
        };
        let delay = backoff.next();
        emit_connection(&emit, false, Some(error), delay);
        sleep(delay).await;
    }
}

fn emit_connection(emit: &impl Fn(&str, Value), reachable: bool, message: Option<String>, delay: Duration) {
    eprintln!("sk1ll-gui: Daemon {}{}", if reachable { "connected" } else { "unreachable" },
        message.as_ref().map(|m| format!(": {m}")).unwrap_or_default());
    let event = ConnectionEvent { reachable, message, retry_in_ms: delay.as_millis() as u64 };
    emit("daemon-connection", serde_json::to_value(event).expect("serializing connection state"));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_caps_and_restores() {
        let mut b = Backoff::default();
        let delays: Vec<_> = (0..10).map(|_| b.next().as_millis()).collect();
        assert_eq!(delays, [500, 1000, 2000, 4000, 8000, 16000, 30000, 30000, 30000, 30000]);
        b.restore();
        assert_eq!(b.next(), MIN_DELAY);
    }
}
