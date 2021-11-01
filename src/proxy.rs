// TODO: Log socket (stream) info
use log::{info, log, trace, Level};
use std::{
    collections::HashMap,
    error::Error,
    fmt,
    io::ErrorKind,
    net::{SocketAddr, ToSocketAddrs},
    sync::{Arc, RwLock},
    time::Duration,
};
use tokio::{
    self,
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
};

const BUFFER_SIZE: usize = 1 << 10;

// Stream read timeout in ms
const STREAM_READ_TIMEOUT: u64 = 1000;

// HTTP/1 Responses
const OK_RESPONSE: &'static [u8] = b"HTTP/1.0 200 OK_RESPONSE\r\n\r\n";
const BAD_REQUEST_RESPONSE: &'static [u8] = b"HTTP/1.0 400 Bad Request\r\n\r\n";
const NOT_FOUND_RESPONSE: &'static [u8] = b"HTTP/1.0 404 Not Found\r\n\r\n";
const INTERNAL_SERVER_ERROR_RESPONSE: &'static [u8] = b"HTTP/1.0 500 Internal Server Error\r\n\r\n";
const VERSION_NOT_SUPPORTED_RESPONSE: &'static [u8] = b"HTTP/1.0 505 Version Not Supported\r\n\r\n";

#[derive(Clone)]
pub struct Proxy {
    addr: SocketAddr,
    servers: Arc<RwLock<HashMap<String, ServerConn>>>,
    //running: AtomicBool,
}

impl Proxy {
    pub fn new(addr: impl ToSocketAddrs) -> Result<Self, Box<dyn Error>> {
        // Convert the given address
        let mut addr_iter;
        match addr.to_socket_addrs() {
            Ok(iter) => addr_iter = iter,
            Err(e) => return Err(Box::new(e)),
        }
        // Get the first address from the iterator
        let addr;
        match addr_iter.next() {
            Some(a) => addr = a,
            _ => panic!("error parsing addr, no address in iterator"),
        }
        // Create the proxy
        trace!("creating new proxy with addr {}", addr.to_string());
        Ok(Proxy {
            addr,
            servers: Arc::new(RwLock::new(HashMap::new())),
        })
    }

    pub async fn run(self) -> Result<(), Box<dyn Error + 'static>> {
        // Create the listener
        // If bind() fails, set the proxy state back to not running
        trace!("attempting to listen on {}", self.addr);
        let listener = match TcpListener::bind(self.addr).await {
            Ok(l) => l,
            Err(e) => return Err(Box::new(e)),
        };
        // Start server-checker function
        let proxy = self.clone();
        tokio::spawn(async move {
            proxy.check_servers().await;
        });
        info!("starting server on {}", self.addr);
        loop {
            let (stream, addr) = listener.accept().await?;
            let proxy = self.clone();
            tokio::spawn(async move {
                proxy.handle(stream, addr).await;
            });
        }
    }

    async fn handle(&self, c_stream: TcpStream, c_addr: SocketAddr) {
        let c_addr_clone = c_addr.to_string().clone();
        let logm = |level, msg| {
            log!(level, "client: {}, server: N/A: {}", c_addr_clone, msg);
        };

        // Wait for the stream to be readable
        logm(Level::Trace, format!("awaiting readable client stream"));
        match tokio::time::timeout(
            Duration::from_millis(STREAM_READ_TIMEOUT),
            c_stream.readable(),
        )
        .await
        {
            Ok(res) => match res {
                Ok(_) => (),
                Err(e) => {
                    logm(
                        Level::Warn,
                        format!("error awaiting readable client stream: {}", e),
                    );
                }
            },
            Err(_) => {
                logm(Level::Trace, format!("client stream read timed out"));
                return;
            }
        }
        // Read from the stream
        let mut buffer = [0u8; BUFFER_SIZE];
        let bytes_read = match c_stream.try_read(&mut buffer) {
            Ok(size) => size,
            Err(e) => {
                logm(
                    Level::Warn,
                    format!("error reading from client stream: {}", e),
                );
                return;
            }
        };
        // Parse the input
        let (method, uri, version) = match parse_request(&buffer[..bytes_read]) {
            Some(parts) => parts,
            None => {
                match c_stream.try_write(BAD_REQUEST_RESPONSE) {
                    Ok(_) => (),
                    Err(e) => logm(
                        Level::Warn,
                        format!("error writing response to client stream: {}", e),
                    ),
                }
                return;
            }
        };
        logm(
            Level::Trace,
            format!(
                "received a `{}` request for path `{}` with HTTP version `{}`",
                method, uri, version
            ),
        );
        // Check the version
        // Only handle HTTP major 1 and minor 0 and 1
        match parse_http_version(version) {
            Some(version) => match version {
                (1, 0) | (1, 1) => (),
                _ => {
                    match c_stream.try_write(VERSION_NOT_SUPPORTED_RESPONSE) {
                        Ok(_) => (),
                        Err(e) => logm(
                            Level::Warn,
                            format!("error writing response to stream: {}", e),
                        ),
                    }
                    return;
                }
            },
            None => {
                match c_stream.try_write(VERSION_NOT_SUPPORTED_RESPONSE) {
                    Ok(_) => (),
                    Err(e) => logm(
                        Level::Warn,
                        format!("error writing response to client stream: {}", e),
                    ),
                }
                return;
            }
        }
        // Handle any special/reserved URIs
        match uri.as_str() {
            "/" => {
                self.handle_home(c_stream, c_addr, logm);
                return;
            }
            "/favicon.ico" => {
                self.handle_favicon(c_stream, c_addr, logm);
                return;
            }
            _ => (),
        }
        // Get the server conn for the given path
        logm(
            Level::Trace,
            format!("getting the server conn for the path {}", uri),
        );
        let server_conn = match self.get_path(uri.clone()) {
            Some(conn) => conn,
            None => {
                match c_stream.try_write(NOT_FOUND_RESPONSE) {
                    Ok(_) => (),
                    Err(e) => logm(
                        Level::Warn,
                        format!("error writing response to client stream: {}", e),
                    ),
                }
                return;
            }
        };
        // Set the server address for logging
        let (c_addr_clone, s_addr_clone) = (
            c_addr.to_string().clone(),
            server_conn.addr.to_string().clone(),
        );
        let logm = move |level, msg| {
            log!(
                level,
                "client: {}, server: {}: {}",
                c_addr_clone,
                s_addr_clone,
                msg
            );
        };
        // Connect to the requested server
        logm(Level::Trace, format!("connecting to the server"));
        let s_stream = match TcpStream::connect(server_conn.addr).await {
            Ok(server) => server,
            Err(e) => {
                // Log client and server stream information
                logm(Level::Warn, format!("error connecting to server: {}", e));
                match c_stream.try_write(INTERNAL_SERVER_ERROR_RESPONSE) {
                    Ok(_) => (),
                    Err(e) => logm(
                        Level::Warn,
                        format!("error writing response to client stream: {}", e),
                    ),
                }
                return;
            }
        };
        // Wait for the server stream to be writable
        match s_stream.writable().await {
            Ok(_) => (),
            Err(e) => {
                logm(
                    Level::Warn,
                    format!("error awaiting writable server stream: {}", e),
                );
                match c_stream.try_write(INTERNAL_SERVER_ERROR_RESPONSE) {
                    Ok(_) => (),
                    Err(e) => logm(
                        Level::Warn,
                        format!("error writing response to client stream: {}", e),
                    ),
                }
                return;
            }
        }
        // Write the request from the server stream
        logm(Level::Trace, format!("writing to the server stream"));
        match s_stream.try_write(&buffer[..bytes_read]) {
            Ok(_) => (),
            Err(e) => {
                logm(
                    Level::Warn,
                    format!("error writing request to server stream: {}", e),
                );
                match c_stream.try_write(INTERNAL_SERVER_ERROR_RESPONSE) {
                    Ok(_) => (),
                    Err(e) => logm(
                        Level::Warn,
                        format!("error writing response to client stream: {}", e),
                    ),
                }
                return;
            }
        }
        // Split the streams
        let (mut c_read, mut c_write) = c_stream.into_split();
        let (mut s_read, mut s_write) = s_stream.into_split();
        // IDEA: Possibly use std::sync::mpsc to just cancel both handles when
        // one sends a message

        // Spawn handle to read from client and write to server
        logm(
            Level::Trace,
            format!("concurrently reading from/writing to the client and server"),
        );
        // Spawn handle to read from client and write to server
        let logmc = logm.clone();
        let (c_read_tx, c_read_rx) = oneshot::channel();
        let c_read_handle = tokio::spawn(async move {
            let mut buffer = [0u8; BUFFER_SIZE];
            loop {
                match c_read.read(&mut buffer).await {
                    Ok(0) => {
                        logmc(Level::Trace, format!("client stream closed"));
                        break;
                    }
                    Ok(size) => match s_write.write(&buffer[..size]).await {
                        Ok(_) => (),
                        Err(e) => {
                            logmc(
                                Level::Warn,
                                format!("error writing to server stream: {}", e),
                            );
                            break;
                        }
                    },
                    Err(e) => {
                        logmc(
                            Level::Warn,
                            format!("error reading from client stream: {}", e),
                        );
                        break;
                    }
                }
            }
            let _ = c_read_tx.send(true);
        });
        // Spawn handle to read from server and write to client
        let logms = logm.clone();
        let (s_read_tx, s_read_rx) = oneshot::channel();
        let s_read_handle = tokio::spawn(async move {
            let mut buffer = [0u8; BUFFER_SIZE];
            loop {
                match s_read.read(&mut buffer).await {
                    Ok(0) => {
                        logms(Level::Trace, format!("server stream closed"));
                        break;
                    }
                    Ok(size) => match c_write.write(&buffer[..size]).await {
                        Ok(_) => (),
                        Err(e) => {
                            logms(
                                Level::Warn,
                                format!("error writing to client stream: {}", e),
                            );
                            break;
                        }
                    },
                    Err(e) => {
                        logms(
                            Level::Warn,
                            format!("error reading from server stream: {}", e),
                        );
                        break;
                    }
                }
            }
            let _ = s_read_tx.send(true);
        });
        // Wait for either handle to exit and cancel the other
        tokio::select! {
            _ = c_read_rx => {
                logm(Level::Trace, format!("client_read_handle done, canceling server_read_handle"));
                s_read_handle.abort();
                match s_read_handle.await {
                    Err(e) if !e.is_cancelled() => {
                        logm(Level::Warn, format!("error cancelling server_read_handle: {}", e));
                        return;
                    },
                    _ => (),
                }
            }
            _ = s_read_rx => {
                logm(Level::Trace, format!("server_read_handle done, cancelling client_read_handle"));
                c_read_handle.abort();
                match c_read_handle.await {
                    Err(e) if !e.is_cancelled() => {
                        logm(Level::Warn, format!("error cancelling client_read_handle: {}", e));
                        return;
                    },
                    _ => (),
                }
            }
        }
        logm(Level::Trace, format!("processes finished"));
    }

    pub fn add_path(&self, server: ServerConn) -> Result<(), ProxyError> {
        trace!(
            "adding server to servers with addr {}://{}",
            server.scheme,
            server.addr
        );
        let servers = Arc::clone(&self.servers);
        let mut servers = servers.write().unwrap();
        if servers.values().any(|v| v.addr == server.addr) {
            return Err(ProxyError {
                what: "Server address already exists".into(),
            });
        }
        servers.insert(server.path.clone(), server);
        Ok(())
    }

    pub fn get_path(&self, path: String) -> Option<ServerConn> {
        let servers = Arc::clone(&self.servers);
        let servers = servers.read().unwrap();
        servers.get(&path).cloned()
    }

    #[allow(dead_code)]
    pub fn get_addr(&self, addr: impl ToSocketAddrs) -> Option<ServerConn> {
        let addr = match addr.to_socket_addrs() {
            Ok(mut addrs) => addrs.next().unwrap(),
            Err(_) => return None,
        };
        let servers = Arc::clone(&self.servers);
        let servers = servers.read().unwrap();
        servers.values().find(|v| v.addr == addr).cloned()
    }

    #[allow(dead_code)]
    pub fn remove_path(&self, path: String) {
        let servers = Arc::clone(&self.servers);
        let mut servers = servers.write().unwrap();
        servers.remove(&path);
    }

    fn handle_home<L>(&self, c_stream: TcpStream, _: SocketAddr, logm: L)
    where
        L: Fn(log::Level, String),
    {
        match c_stream.try_write(OK_RESPONSE) {
            Ok(_) => (),
            Err(e) => logm(
                Level::Warn,
                format!("error writing response to client stream: {}", e),
            ),
        }
    }

    fn handle_favicon<L>(&self, c_stream: TcpStream, _: SocketAddr, logm: L)
    where
        L: Fn(log::Level, String),
    {
        match c_stream.try_write(NOT_FOUND_RESPONSE) {
            Ok(_) => (),
            Err(e) => logm(
                Level::Warn,
                format!("error writing response to client stream: {}", e),
            ),
        }
    }

    async fn check_servers(&self) {
        loop {
            tokio::time::sleep(Duration::new(5, 0)).await;
            let servers = Arc::clone(&self.servers);
            let servers_iter = { servers.read().unwrap().clone().into_iter() };
            *servers.write().unwrap() = servers_iter
                .filter_map(|(path, mut conn)| {
                    // IDEA: Use connection timeout
                    match std::net::TcpStream::connect(conn.addr.clone()) {
                        Ok(_) => Some((path, conn)),
                        Err(ref e) if e.kind() != ErrorKind::ConnectionRefused => {
                            Some((path, conn))
                        }
                        Err(_) => {
                            if conn.failed {
                                None
                            } else {
                                conn.failed = true;
                                Some((path, conn))
                            }
                        }
                    }
                })
                .collect();
        }
    }
}

#[derive(Clone)]
pub struct ServerConn {
    path: String,
    addr: SocketAddr,
    scheme: String,
    failed: bool,
}

impl ServerConn {
    pub fn new(path: String, addr: impl ToSocketAddrs, scheme: String) -> Option<Self> {
        let addr = match addr.to_socket_addrs() {
            Ok(mut addrs) => addrs.next().unwrap(),
            Err(_) => return None,
        };
        Some(Self {
            path,
            scheme,
            addr,
            failed: false,
        })
    }
}

// Returns the request's (method, URI, HTTP Version [rest of first line])
// Returns none if the request if malformed
fn parse_request(request: &[u8]) -> Option<(String, String, String)> {
    let index;
    match request.iter().position(|&b| b == b'\r') {
        Some(i) => index = i,
        None => return None,
    }
    let first_line = String::from_utf8_lossy(&request[..index]);
    let parts = first_line.split_whitespace().collect::<Vec<&str>>();
    if parts.len() < 3 {
        return None;
    }
    Some((
        parts[0].to_owned(),
        parts[1].to_owned(),
        parts[2..].join(" ").to_owned(),
    ))
}

// Parses an HTTP version
// Currently only parses and accepts HTTP/1 requests
fn parse_http_version(version: String) -> Option<(i32, i32)> {
    let version = version.as_bytes();
    match version {
        b"HTTP/1.0" => return Some((1, 0)),
        b"HTTP/1.1" => return Some((1, 1)),
        _ => (),
    }
    if !version.starts_with(b"HTTP/") {
        return None;
    }
    let pos;
    match version.iter().position(|&b| b == b'.') {
        Some(p) => pos = p,
        None => return None,
    }
    let (major, minor);
    match String::from_utf8_lossy(&version[5..pos]).parse::<i32>() {
        Ok(i) => major = i,
        Err(_) => return None,
    }
    match String::from_utf8_lossy(&version[pos + 1..]).parse::<i32>() {
        Ok(i) => minor = i,
        Err(_) => return None,
    }
    Some((major, minor))
}

#[derive(Debug)]
pub struct ProxyError {
    what: String,
}

impl fmt::Display for ProxyError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Proxy error: {}", self.what)
    }
}

impl Error for ProxyError {}
