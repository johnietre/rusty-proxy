// TODO: Log socket (stream) info
#![allow(dead_code)]
use log::{info, trace, warn};
use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};

const BUFFER_SIZE: usize = 1 << 12;

// Stream read timeout in ms
const STREAM_READ_TIMEOUT: u64 = 1000;

const OK: &'static str = "HTTP/1.0 200 OK\r\n\r\n";
const BAD_REQUEST: &'static str = "HTTP/1.0 400 Bad Request\r\n\r\n";
const NOT_FOUND: &'static str = "HTTP/1.0 404 Not Found\r\n\r\n";
const VERSION_NOT_SUPPORTED: &'static str = "HTTP/1.0 505 Version Not Supported\r\n\r\n";

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

    async fn handle(&self, stream: TcpStream, addr: SocketAddr) {
        let addr_str = addr.to_string();
        trace!("handling connection from {}", addr_str);

        // Change the stream into a standard library TcpStream
        trace!("converting tokio::TcpStream into std::TcpStream");
        let mut std_stream = match stream.into_std() {
            Ok(ss) => ss,
            Err(e) => {
                warn!("error converting stream: {}", e);
                return;
            },
        }
        // Change it to blocking
        trace!("changing stream to nonblocking");
        match std_stream.set_nonblocking(false) {
            Ok(_) => (),
            Err(e) => {
                warn!("error setting to nonblocking: {}", e);
                return;
            },
        }
        // Set the read timeout
        trace!("setting stream read timeout to {} ms", STREAM_READ_TIMEOUT);
        match std_stream.set_read_timeout(Some(Duration::from_millis(STREAM_READ_TIMEOUT))) {
            Ok(_) => (),
            Err(e) => {
                warn!("error setting read timeout: {}", e);
                return;
            },
        }
        // Read from the stream
        trace!("reading from the stream");
        #[allow(unused_mut)]
        let mut buffer = [0u8; BUFFER_SIZE];
        let mut bytes_read = match std_stream.read(&mut buffer) {
            Ok(size) => size,
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut => {
                warn!("stream read timed out");
                return;
            },
            Err(e) => {
                warn!("error reading from stream: {}", e);
                return;
            },
        };
        // Parse the input
        trace!("parsing data from the stream");
        #[allow(unused_variables)]
        #[allow(unused_assignments)]
        let (method, uri, version) = match parse_request(&buffer[..bytes_read]) {
            Some(parts) => parts
            None => {
                match std_stream.write(BAD_REQUEST.as_bytes()) {
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
                    match std_stream.write(VERSION_NOT_SUPPORTED.as_bytes()) {
                        Ok(_) => (),
                        Err(e) => warn!("error writing response to stream: {}", e),
                    }
                    return;
                }
            },
            None => {
                match std_stream.write(VERSION_NOT_SUPPORTED.as_bytes()) {
                    Ok(_) => (),
                    Err(e) => warn!("error writing response to stream: {}", e),
                }
                return;
            }
        }
        // Get the server conn for the given path
        trace!("getting the server conn for the path {}", uri);
        #[allow(unused_assignments)]
        #[allow(unreachable_patterns)]
        let server_conn = match self.get_path(uri.clone()) {
            Some(server) => server,
            None => {
                match std_stream.write(NOT_FOUND.as_bytes()) {
                    Ok(_) => (),
                    Err(e) => warn!("error writing response to stream: {}", e),
                }
                return;
            },
        };
        if bytes_read == BUFFER_SIZE {
            info!("more bytes in stream");
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
