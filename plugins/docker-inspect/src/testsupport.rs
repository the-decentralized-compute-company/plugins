//! A stub that speaks just enough of the Docker Engine API to drive the real
//! request path in tests.
//!
//! The alternative — mocking the client — would prove only that the mock was
//! called. This serves canned bodies over a real loopback socket, so the tests
//! exercise connecting, the `GET` this plugin actually writes, response
//! framing, status handling, and decoding, and can assert on the request line
//! the daemon would have seen.
//!
//! It listens on TCP because that is the one transport available on every
//! platform the plugin builds for; the Unix-socket and named-pipe paths differ
//! only in how the stream is opened.

use std::sync::{Arc, Mutex};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::endpoint::Endpoint;
use crate::settings::{EnvMap, Settings};

/// A canned `(status, body)` pair, one per connection, in order.
pub type CannedResponse = (u16, String);

pub fn ok(body: &str) -> CannedResponse {
    (200, body.to_string())
}

pub struct StubDaemon {
    port: u16,
    requests: Arc<Mutex<Vec<String>>>,
}

impl StubDaemon {
    /// Serve one queued response per incoming connection, recording each
    /// request line. Runs on its own thread and runtime so a test can drive it
    /// from any runtime flavour.
    pub fn spawn(responses: Vec<CannedResponse>) -> Self {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();

        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("the stub's runtime builds");
            runtime.block_on(async move {
                let listener = TcpListener::bind("127.0.0.1:0").await.expect("binds");
                ready_tx
                    .send(listener.local_addr().expect("has an address").port())
                    .expect("the test is waiting for the port");

                for (status, body) in responses {
                    let Ok((stream, _)) = listener.accept().await else {
                        return;
                    };
                    let (reader, mut writer) = stream.into_split();
                    let mut reader = tokio::io::BufReader::new(reader);

                    let mut request_line = String::new();
                    if reader.read_line(&mut request_line).await.is_err() {
                        return;
                    }
                    loop {
                        let mut line = String::new();
                        match reader.read_line(&mut line).await {
                            Ok(0) | Err(_) => break,
                            Ok(_) if line == "\r\n" => break,
                            Ok(_) => {}
                        }
                    }
                    recorded
                        .lock()
                        .expect("no test panicked while holding this lock")
                        .push(request_line.trim().to_string());

                    let response = format!(
                        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = writer.write_all(response.as_bytes()).await;
                    let _ = writer.flush().await;
                }
            });
        });

        Self {
            port: ready_rx.recv().expect("the stub binds"),
            requests,
        }
    }

    pub fn endpoint(&self) -> Endpoint {
        Endpoint::Tcp {
            host: "127.0.0.1".into(),
            port: self.port,
        }
    }

    /// Settings pointing at this stub, with any extra flags a test needs.
    ///
    /// Built through the real parser rather than by filling the struct in, so a
    /// test cannot construct a configuration the operator could not.
    pub fn settings(&self, extra: &[&str]) -> Settings {
        let mut args = vec![
            "--endpoint".to_string(),
            format!("tcp://127.0.0.1:{}", self.port),
            "--allow-tcp".to_string(),
        ];
        args.extend(extra.iter().map(|value| (*value).to_string()));
        Settings::parse(&args, &EnvMap::new()).expect("the test configuration parses")
    }

    /// The request lines the stub received, in order.
    pub fn requests(&self) -> Vec<String> {
        self.requests
            .lock()
            .expect("no test panicked while holding this lock")
            .clone()
    }
}
