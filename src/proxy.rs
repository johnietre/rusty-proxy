// TODO: Log socket (stream) info
use log::{info, warn};
use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::{Arc, RwLock, atomic::{AtomicBool, Ordering}};
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};

const BUFFER_SIZE: usize = 1 << 12;

// Stream read timeout in ms
const STREAM_READ_TIMEOUT: u64 = 100;

const BAD_REQUEST: &'static str = "HTTP/1.0 400 Bad Request\r\n\r\n";
const NOT_FOUND: &[u8] = b"HTTP/1.0 404 Not Found\r\n\r\n";
const VERSION_NOT_SUPPORTED: &'static str = "HTTP/1.0 505 Version Not Supported\r\n\r\n";

pub struct Proxy {
    addr: SocketAddr,
    routes: Arc<RwLock<HashMap<String, Route>>>,
    running: AtomicBool,
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
            _ => panic!("Error parsing addr, no address in iterator"),
        }
        // Create the proxy
        Ok(Proxy {
            addr,
            routes: Arc::new(RwLock::new(HashMap::new())),
            running: AtomicBool::new(false),
        })
    }

    pub async fn run(&self) -> Result<(), Box<dyn Error>> {
        // TODO: Set running to false if bind() fails
        match self.running.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed) {
            Ok(_) => (),
            Err(_) => return Err(Box::new(ProxyError {
                what: "Proxy already running".into(),
            })),
        }
        // Create the listener
        //let listener = TcpListener::bind(self.addr).await?;
        let listener = match TcpListener::bind(self.addr).await {
            Ok(l) => l,
            Err(e) => {
                self.running.store(true, Ordering::Relaxed);
                return Err(Box::new(e));
            },
        };
        info!(" Starting server on {}", self.addr);
        loop {
            let (stream, addr) = listener.accept().await?;
            self.handle(stream, addr);
        }
    }

    async fn handle(&self, stream: TcpStream, addr: SocketAddr) {
        let mut buffer = [0u8; BUFFER_SIZE];
        // Change the stream into a standard library TcpStream
        let mut std_stream;
        match stream.into_std() {
            Ok(ss) => std_stream = ss,
            Err(e) => {
                warn!("Error converting stream: {}", e);
                return;
            },
        }
        // Change it to blocking
        match std_stream.set_nonblocking(false) {
            Ok(_) => (),
            Err(e) => {
                warn!("Error setting to nonblocking: {}", e);
                return;
            },
        }
        // Set the read timeout
        match std_stream.set_read_timeout(Some(Duration::from_millis(STREAM_READ_TIMEOUT))) {
            Ok(_) => (),
            Err(e) => {
                warn!("Error setting read timeout: {}", e);
                return;
            },
        }
        // Read from the stream
        let mut bytes_read;
        match std_stream.read(&mut buffer) {
            Ok(size) => bytes_read = size,
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut => {
                warn!("Stream read timed out");
                return;
            },
            Err(e) => {
                warn!("Error reading from stream: {}", e);
                return;
            },
        }
        // Parse the input
        let (method, uri, version);
        match parse_request(&buffer) {
            Some((m, u, v)) => {
                method = m;
                uri = u;
                version = v;
            },
            None => {
                match std_stream.write(BAD_REQUEST.as_bytes()) {
                    Ok(_) => (),
                    Err(e) => warn!("Error writing response to stream: {}", e),
                }
                return;
            }
        }
        // Check the version
        // Only handle HTTP major 1 and minor 0 and 1
        match parse_http_version(version) {
            Some(version) => match version {
                (1, 0) | (1, 1) => (),
                _ => {
                    match std_stream.write(VERSION_NOT_SUPPORTED.as_bytes()) {
                        Ok(_) => (),
                        Err(e) => warn!("Error writing response to stream: {}", e),
                    }
                    return;
                }
            },
            None => {
                match std_stream.write(VERSION_NOT_SUPPORTED.as_bytes()) {
                    Ok(_) => (),
                    Err(e) => warn!("Error writing response to stream: {}", e),
                }
                return;
            }
        }
        let route;
        match self.get_handler(uri) {
            Some(r) => route = r,
            None => {
                match std_stream.write(NOT_FOUND) {
                    Ok(_) => (),
                    Err(e) => warn!("Error writing response to stream: {}", e),
                }
                return;
            },
        }
        if bytes_read == BUFFER_SIZE {
            info!("More");
        }
        info!("Route: {}", route.path.clone());
    }

    pub fn add_handler(&self, path: String, handler: Handler) {
        let routes = Arc::clone(&self.routes);
        let mut routes = routes.write().unwrap();
        routes.insert(path.clone(), Route::new(path, handler));
    }

    pub fn add_route(&self, route: Route) {
        let routes = Arc::clone(&self.routes);
        let mut routes = routes.write().unwrap();
        routes.insert(route.path.clone(), route);
    }

    pub fn get_handler(&self, path: String) -> Option<Route> {
        let routes = Arc::clone(&self.routes);
        let routes = routes.read().unwrap();
        routes.get(&path).cloned()
    }

    pub fn remove_path(&self, path: String) {
        let routes = Arc::clone(&self.routes);
        let mut routes = routes.write().unwrap();
        routes.remove(&path);
    }
}

#[derive(Clone)]
pub struct Route {
    path: String,
    server_addr: String,
    scheme: String,
    handler: Handler,
}

impl Route {
    pub fn new(path: String, handler: Handler) -> Self {
        Self {
            path,
            handler,
            scheme: "http".into(),
            server_addr: String::new(),
        }
    }
}

// TcpStream is the client stream and the slice is the request bytes
type Handler = fn(TcpStream, &[u8]);

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
