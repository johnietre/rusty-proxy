// TODO: Log socket (stream) info
#![allow(dead_code)]
use log::{info, trace, warn};
use std::{
    collections::HashMap,
    error::Error,
    fmt,
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

const BUFFER_SIZE: usize = 1 << 12;

// Stream read timeout in ms
const STREAM_READ_TIMEOUT: u64 = 1000;

const OK_RESPONSE: &'static str = "HTTP/1.0 200 OK_RESPONSE\r\n\r\n";
const BAD_REQUEST_RESPONSE: &'static str = "HTTP/1.0 400 Bad Request\r\n\r\n";
const NOT_FOUND_RESPONSE: &'static str = "HTTP/1.0 404 Not Found\r\n\r\n";
const INTERNAL_SERVER_ERROR_RESPONSE: &'static str = "HTTP/1.0 500 Internal Server Error\r\n\r\n";
const VERSION_NOT_SUPPORTED_RESPONSE: &'static str = "HTTP/1.0 505 Version Not Supported\r\n\r\n";

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
        let c_addr_str = c_addr.to_string();
        trace!("handling connection from {}", c_addr_str);

        // Wait for the stream to be readable
        match c_stream.readable().await {
            Ok(_) => (),
            Err(e) => {
                warn!("error awaiting readable stream: {}", e);
                return;
            },
        }
        // Read from the stream
        trace!("reading from the stream");
        #[allow(unused_mut)]
        let mut buffer = [0u8; BUFFER_SIZE];
        let bytes_read = match tokio::time::timeout(Duration::from_millis(STREAM_READ_TIMEOUT), async { c_stream.try_read(&mut buffer) }).await {
            Ok(res) => match res {
                Ok(size) => size,
                Err(e) => {
                    warn!("error reading from stream: {}", e);
                    return;
                },
            },
            Err(_) => {
                warn!("stream read timed out");
                return;
            },
        };
        // Parse the input
        trace!("parsing data from the stream");
        #[allow(unused_variables)]
        #[allow(unused_assignments)]
        let (method, uri, version) = match parse_request(&buffer[..bytes_read]) {
            Some(parts) => parts,
            None => {
                match c_stream.try_write(BAD_REQUEST_RESPONSE.as_bytes()) {
                    Ok(_) => (),
                    Err(e) => warn!("error writing response to stream: {}", e),
                }
                return;
            }
        };
        // Check the version
        // Only handle HTTP major 1 and minor 0 and 1
        trace!("parsing the http version");
        match parse_http_version(version) {
            Some(version) => match version {
                (1, 0) | (1, 1) => (),
                _ => {
                    match c_stream.try_write(VERSION_NOT_SUPPORTED_RESPONSE.as_bytes()) {
                        Ok(_) => (),
                        Err(e) => warn!("error writing response to stream: {}", e),
                    }
                    return;
                }
            },
            None => {
                match c_stream.try_write(VERSION_NOT_SUPPORTED_RESPONSE.as_bytes()) {
                    Ok(_) => (),
                    Err(e) => warn!("error writing response to stream: {}", e),
                }
                return;
            }
        }
        // Get the server conn for the given path
        trace!("getting the server conn for the path {}", uri);
        let server_conn = match self.get_path(uri.clone()) {
            Some(conn) => conn,
            None => {
                match c_stream.try_write(NOT_FOUND_RESPONSE.as_bytes()) {
                    Ok(_) => (),
                    Err(e) => warn!("error writing response to stream: {}", e),
                }
                return;
            },
        };
        // Connect to the requested server
        trace!("connecting to the requested server");
        let s_stream = match TcpStream::connect(server_conn.addr).await {
            Ok(server) => server,
            Err(e) => {
                // Log client and server stream information
                warn!("error connecting to server: {}", e);
                match c_stream.try_write(INTERNAL_SERVER_ERROR_RESPONSE.as_bytes()) {
                    Ok(_) => (),
                    Err(e) => warn!("error writing response to client stream: {}", e),
                }
                return;
            },
        };
        // Wait for the server stream to be writable
        match s_stream.writable().await {
            Ok(_) => (),
            Err(e) => {
                warn!("error awaiting writable server stream: {}", e);
                match c_stream.try_write(INTERNAL_SERVER_ERROR_RESPONSE.as_bytes()) {
                    Ok(_) => (),
                    Err(e) => warn!("error writing response to client stream: {}", e),
                }
                return;
            },
        }
        // Write the request from the server stream
        trace!("writing to the server stream");
        match s_stream.try_write(&buffer[..bytes_read]) {
            Ok(_) => (),
            Err(e) => {
                warn!("error writing request to server stream: {}", e);
                match c_stream.try_write(INTERNAL_SERVER_ERROR_RESPONSE.as_bytes()) {
                    Ok(_) => (),
                    Err(e) => warn!("error writing response to client stream: {}", e),
                }
                return;
            },
        }
        // Split the streams
        trace!("splitting the client and server streams");
        let (mut c_read, mut c_write) = c_stream.into_split();
        let (mut s_read, mut s_write) = s_stream.into_split();
        // IDEA: Possibly use std::sync::mpsc to just cancel both handles when
        // one sends a message

        // Spawn handle to read from client and write to server
        let (c_read_tx, c_read_rx) = oneshot::channel();
        trace!("concurrently reading from/writing to the client and server");
        let c_read_handle = tokio::spawn(async move {
            let mut buffer = [0u8; BUFFER_SIZE];
            loop {
                match c_read.read(&mut buffer).await {
                    Ok(size) => match s_write.write(&buffer[..size]).await {
                        Ok(_) => (),
                        Err(e) => {
                            warn!("error writing to server stream: {}", e);
                            break;
                        },
                    },
                    Err(e) => {
                        warn!("error reading from client stream: {}", e);
                        break;
                    }
                }
            }
            let _ = c_read_tx.send(true);
        });
        // Spawn handle to read from server and write to client
        let (s_read_tx, s_read_rx) = oneshot::channel();
        let s_read_handle = tokio::spawn(async move {
            let mut buffer = [0u8; BUFFER_SIZE];
            loop {
                match s_read.read(&mut buffer).await {
                    Ok(size) => match c_write.write(&buffer[..size]).await {
                        Ok(_) => (),
                        Err(e) => {
                            warn!("error writing to client stream: {}", e);
                            break;
                        },
                    },
                    Err(e) => {
                        warn!("error reading from server stream: {}", e);
                        break;
                    },
                }
            }
            let _ = s_read_tx.send(true);
        });
        // Wait for either handle to exit and cancel the other
        tokio::select! {
            _ = c_read_rx => {
                trace!("client_read_handle done, canceling server_read_handle");
                s_read_handle.abort();
                match s_read_handle.await {
                    Err(e) if !e.is_cancelled() => {
                        warn!("error cancelling server_read_handle: {}", e);
                        return;
                    },
                    _ => (),
                }
            }
            _ = s_read_rx => {
                trace!("server_read_handle done, cancelling client_read_handle");
                c_read_handle.abort();
                match c_read_handle.await {
                    Err(e) if !e.is_cancelled() => {
                        warn!("error cancelling client_read_handle: {}", e);
                        return;
                    },
                    _ => (),
                }
            }
        }
    }

    pub fn add_path(&self, server: ServerConn) -> Result<(), ProxyError> {
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

    pub fn get_addr(&self, addr: String) -> Option<ServerConn> {
        let servers = Arc::clone(&self.servers);
        let servers = servers.read().unwrap();
        servers.values().find(|v| v.addr == addr).cloned()
    }

    pub fn remove_path(&self, path: String) {
        let servers = Arc::clone(&self.servers);
        let mut servers = servers.write().unwrap();
        servers.remove(&path);
    }
}

#[derive(Clone)]
pub struct ServerConn {
    path: String,
    // addr: SocketAddr
    addr: String,
    scheme: String,
}

impl ServerConn {
    pub fn new(path: String, addr: String, scheme: String) -> Self {
        Self {
            path,
            addr,
            scheme,
        }
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
    Some((parts[0].to_owned(), parts[1].to_owned(), parts[2..].join(" ").to_owned()))
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
    match String::from_utf8_lossy(&version[pos+1..]).parse::<i32>() {
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
