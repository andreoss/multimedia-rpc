use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

use crate::registry::Registry;

const READ_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_REQUEST_BYTES: usize = 4096;
const CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

pub struct Scrape {
    local_addr: SocketAddr,
    accept: JoinHandle<()>,
}

impl Scrape {
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub async fn shutdown(self) {
        self.accept.abort();
        let _ = self.accept.await;
    }
}

pub async fn serve(registry: Arc<Registry>, bind: SocketAddr) -> Result<Scrape, std::io::Error> {
    let listener = TcpListener::bind(bind).await?;
    let local_addr = listener.local_addr()?;
    let accept = tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let registry = registry.clone();
            tokio::spawn(async move {
                answer(registry, stream).await;
            });
        }
    });
    Ok(Scrape { local_addr, accept })
}

async fn answer(registry: Arc<Registry>, mut stream: TcpStream) {
    let mut buffer = vec![0u8; MAX_REQUEST_BYTES];
    let read = match tokio::time::timeout(READ_TIMEOUT, stream.read(&mut buffer)).await {
        Ok(Ok(read)) => read,
        _ => return,
    };
    let request = String::from_utf8_lossy(&buffer[..read]).to_string();
    let mut parts = request.split_whitespace();
    let method = parts.next().unwrap_or_default();
    let path = parts.next().unwrap_or_default();
    let metrics = registry.render();
    let response = if method == "GET" && (path == "/metrics" || path == "/") {
        compose("200 OK", &metrics)
    } else {
        compose("404 Not Found", "not found\n")
    };
    let _ = tokio::time::timeout(READ_TIMEOUT, stream.write_all(response.as_bytes())).await;
    let _ = stream.shutdown().await;
}

fn compose(status: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\ncontent-type: {CONTENT_TYPE}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
}
