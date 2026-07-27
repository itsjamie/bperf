use std::{
    io::{self, Read as _, Write as _},
    net::{Shutdown, TcpListener, TcpStream},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

const ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_REQUEST_HEADER_BYTES: usize = 64 * 1024;
const DOCUMENT: &[u8] = br#"<!doctype html><script>
const previous = localStorage.getItem("bperf-context");
localStorage.setItem("bperf-context", "used");
globalThis.__bperfDescription = { fresh: previous === null };
globalThis.__bperf = {
  run() {
    let total = 0;
    const deadline = performance.now() + 100;
    while (performance.now() < deadline) {
      for (let index = 0; index < 10_000; index += 1) {
        total += Math.sqrt(index % 1_000);
      }
    }
    globalThis.__bperfParityHeap ??= [];
    globalThis.__bperfParityHeap.push(new Array(1_000).fill(total));
    return previous === null;
  }
};
</script>"#;

pub(crate) struct FreshContextServer {
    url: String,
    running: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl FreshContextServer {
    pub(crate) fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let running = Arc::new(AtomicBool::new(true));
        let thread_running = Arc::clone(&running);
        let thread = thread::spawn(move || {
            while thread_running.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        if !thread_running.load(Ordering::Acquire) {
                            break;
                        }
                        let _ = serve_document(stream);
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(ACCEPT_POLL_INTERVAL);
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            url: format!("http://{address}/"),
            running,
            thread: Some(thread),
        }
    }

    pub(crate) fn url(&self) -> &str {
        &self.url
    }
}

impl Drop for FreshContextServer {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Release);
        let _ = TcpStream::connect(self.url.trim_end_matches('/').trim_start_matches("http://"));
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn serve_document(mut stream: TcpStream) -> io::Result<()> {
    // Windows accepts a socket with the listener's nonblocking mode. Restore a
    // bounded blocking read so a connection cannot be answered before its
    // request arrives or prevent the fixture thread from shutting down.
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(REQUEST_TIMEOUT))?;
    read_request_headers(&mut stream)?;

    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        DOCUMENT.len()
    )?;
    stream.write_all(DOCUMENT)?;
    stream.flush()?;
    stream.shutdown(Shutdown::Write)
}

fn read_request_headers(stream: &mut TcpStream) -> io::Result<()> {
    let mut request = Vec::with_capacity(1024);
    let mut chunk = [0_u8; 1024];
    loop {
        let count = stream.read(&mut chunk)?;
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "client closed before sending complete HTTP headers",
            ));
        }
        request.extend_from_slice(&chunk[..count]);
        if request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
            return Ok(());
        }
        if request.len() >= MAX_REQUEST_HEADER_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "HTTP request headers exceed the fixture limit",
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waits_for_complete_request_headers_before_responding() {
        let server = FreshContextServer::start();
        let mut stream = TcpStream::connect(
            server
                .url()
                .trim_end_matches('/')
                .trim_start_matches("http://"),
        )
        .unwrap();
        stream
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        thread::sleep(Duration::from_millis(25));

        let mut response = Vec::new();
        let early_read = stream.read_to_end(&mut response);
        assert!(
            early_read.as_ref().is_err_and(|error| matches!(
                error.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
            )),
            "server responded before receiving a request: {early_read:?}"
        );

        stream
            .write_all(b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
            .unwrap();
        response.clear();
        stream.read_to_end(&mut response).unwrap();
        assert!(response.starts_with(b"HTTP/1.1 200 OK\r\n"));
        assert!(response.ends_with(DOCUMENT));
    }
}
