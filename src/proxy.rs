// TODO: Also create parse_requset function that parses and reformats a request
// that takes an optional server name, checking if the request is asking for
// the same server, if not, connecting to the new server
// TODO: Requests with no server name (i.e., no path) cause a thread panic from
// regex becuase there is no group index at 1
// TODO: Take logm closures in all "serve_" functions
// TODO: Change #[allow(unused_must_used)] to let _ = ...
// TODO: Handle stream closed errors differently
#![allow(dead_code)]
#![allow(unused_imports)]
use either::Either;
use lazy_static::lazy_static;
use log::{log, Level};
use regex::bytes::Regex; // TODO: Possibly use regular str version
use socket2::{Domain, Protocol, Socket, Type};
use std::io::{self, Write};
use std::marker::Unpin;
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::{
    atomic::{AtomicBool, AtomicU32, Ordering},
    Arc,
};
use std::time::Duration;
use tokio::io::{
    self as tio, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader,
    BufWriter, ReadHalf, WriteHalf,
};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::rustls;
use tokio_rustls::{TlsAcceptor, TlsConnector};

use rusty_proxy::sync_map::SyncMap;

const BUFFER_SIZE: usize = 1 << 10;
const STREAM_READ_TIMEOUT: u64 = 10_000; // In ms

// NOTE: Add other headers like Content-Type and Connection: close ?
const BAD_REQUEST_RESPONSE: &'static [u8] = b"HTTP/1.1 400 400 Bad Request\r\n\r\n";
const NOT_FOUND_RESPONSE: &'static [u8] = b"HTTP/1.1 404 Not Found\r\n\r\n";
const INTERNAL_SERVER_ERROR_RESPONSE: &'static [u8] = b"HTTP/1.1 500 Internal Server Error\r\n\r\n";

lazy_static! {
    static ref REQ_RE: Regex = Regex::new(r"^\w+ /([^/ ]+)?(/.+)? HTTP").unwrap();
}

trait IOUnpin: AsyncRead + AsyncWrite + Unpin + Send {}

impl IOUnpin for TcpStream {}
impl IOUnpin for tokio_rustls::server::TlsStream<TcpStream> {}

#[derive(Clone)]
pub struct Proxy {
    acceptor: Option<TlsAcceptor>,
    connector: Option<TlsConnector>,
    addr: SocketAddr,
    server_info_map: SyncMap<ServerInfo>,
    shutdown: Arc<AtomicBool>,
    force_shutdown: Arc<AtomicBool>,
    status: Arc<ProxyStatus>,
    num_running: Arc<AtomicU32>,
    tunnel_reqs: Arc<AtomicU32>,
    command_password: Arc<String>,
    //reserved_paths: Arc<HashMap<String, Box<dyn Fn()>>>,
}

impl Proxy {
    pub fn new<A: ToSocketAddrs>(addr: A) -> io::Result<Self> {
        let addr = addr
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| io::Error::from(io::ErrorKind::AddrNotAvailable))?;
        Ok(Self {
            acceptor: None,
            connector: None,
            addr: addr,
            server_info_map: {
                let m = SyncMap::new();
                let server = ServerInfo::new("server1", "127.0.0.1:9000");
                m.store(server.name.clone(), server);
                let server = ServerInfo::new("server2", "127.0.0.1:9001");
                m.store(server.name.clone(), server);
                let server = ServerInfo::new("server3", "127.0.0.1:9002");
                m.store(server.name.clone(), server);
                let server = ServerInfo::new("server4", "127.0.0.1:9003");
                m.store(server.name.clone(), server);
                m
            },
            shutdown: Arc::new(AtomicBool::new(false)),
            force_shutdown: Arc::new(AtomicBool::new(false)),
            status: Arc::new(ProxyStatus::NotStarted),
            num_running: Arc::new(AtomicU32::new(0)),
            tunnel_reqs: Arc::new(AtomicU32::new(0)),
            command_password: Arc::new(String::new()),
            //reserved_paths: Arc::new(HashMap::new()),
        })
    }

    pub async fn run(&self) -> io::Result<()> {
        let socket = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP))?;
        socket.set_reuse_port(true)?;
        let addr = self.addr.clone().into();
        socket.bind(&addr)?;
        socket.listen(128)?;
        // TODO: Possibly use socket.listen function
        socket.set_nonblocking(true)?;
        let ln = TcpListener::from_std(socket.into())?;
        log!(Level::Trace, "listening on {}", addr.as_socket().unwrap());
        loop {
            // TODO: Poll accept while watching proxy status; possibly use oneshot channel
            match ln.accept().await {
                Ok((conn, addr)) => {
                    let px = self.clone();
                    tokio::spawn(async move {
                        px.accept(conn, addr).await;
                    });
                }
                Err(err) => {
                    // TODO: Handle different errors
                    //error!("error accepting connection: {}", err);
                    break Err(err);
                }
            }
        }
    }

    async fn accept(self, client: TcpStream, addr: SocketAddr) {
        if let Some(acceptor) = self.acceptor.as_ref() {
            // TODO: Possibly send 426 upgrade response on error
            match acceptor.accept(client).await {
                Ok(client) => self.handle(client, addr).await,
                Err(e) => eprintln!("error accepting client TLS: {}", e), // TODO: Handle error better
            }
        } else {
            self.handle(client, addr).await;
        }
    }

    // NOTE: Won't send Forwarded header on subsequent writes that aren't successfully parsed
    async fn handle<S: IOUnpin>(self, client: S, client_addr: SocketAddr) {
        let client_addr_clone = client_addr.clone();
        let logm = |level, msg| {
            log!(level, "client: {}, server: N/A: {}", client_addr_clone, msg);
        };
        let mut req_buf = [0u8; BUFFER_SIZE];
        let (ref mut client_read, ref mut client_write) = tio::split(client);
        let mut bytes_read = 0usize;
        loop {
            // If bytes_read is 0, read in a new request
            if bytes_read == 0 {
                // Read from the stream with the timeout
                bytes_read = match tokio::time::timeout(
                    Duration::from_millis(STREAM_READ_TIMEOUT),
                    client_read.read(&mut req_buf),
                )
                .await
                {
                    Ok(res) => match res {
                        #[allow(unused_must_use)] // Used for the write below
                        Ok(l) if l < 2 => {
                            client_write.write(BAD_REQUEST_RESPONSE).await;
                            return;
                        }
                        Ok(l) => l,
                        Err(e) => {
                            // TODO: Handle closed conn differently (don't log)
                            logm(
                                Level::Info,
                                format!("error reading from client stream: {}", e),
                            );
                            return;
                        }
                    },
                    Err(_) => {
                        logm(Level::Trace, String::from("client stream read timed out"));
                        return;
                    }
                };
            }
            // Check if the request is a special request
            match SpecialReqHeader::from(&req_buf[..2]) {
                SpecialReqHeader::Tunnel => (),  // TODO: Handle tunnel
                SpecialReqHeader::Command => (), // TODO: Handle command
                SpecialReqHeader::Unknown => (),
            }
            let req = &req_buf[..bytes_read];
            let (site_name, first_line) = match Self::parse_site_name(req).await {
                Ok(name) => name,
                #[allow(unused_must_use)]
                Err(e) => {
                    client_write.write(e).await;
                    return;
                }
            };
            // Check if the site is reserved
            match site_name.as_str() {
                "" => {
                    self.serve_home(client_write, req).await;
                    continue;
                }
                "favicon.ico" => {
                    self.serve_favicon(client_write, req).await;
                    continue;
                }
                _ => (),
            }

            // Get the server info
            let server_info = match self.server_info_map.load(&site_name) {
                Some(info) => info,
                #[allow(unused_must_use)]
                None => {
                    client_write.write(NOT_FOUND_RESPONSE).await;
                    return;
                }
            };
            let server_addr_clone = server_info.addr.clone();
            // Update the logm closure
            let logm = |level, msg| {
                log!(
                    level,
                    "client: {}, server: {}: {}",
                    client_addr_clone,
                    server_addr_clone,
                    msg
                );
            };
            if server_info.tunnel {
                // TODO
                return;
            }
            // Connect to the server
            let server = match TcpStream::connect(&server_info.addr).await {
                Ok(stream) => stream,
                #[allow(unused_must_use)]
                Err(e) => {
                    logm(Level::Info, format!("error connecting to server: {}", e));
                    client_write.write(INTERNAL_SERVER_ERROR_RESPONSE).await;
                    return;
                }
            };
            let server: Box<dyn IOUnpin> = if server_info.secure {
                if let Some(_connector) = self.connector.clone() {
                    // TODO: Connect
                    Box::new(server)
                } else {
                    // TODO: Log error (error is that there needs to be a client connector
                    return;
                }
            } else {
                Box::new(server)
            };

            // Write the headers to a buffer, adding/changing things when needed
            let mut buf_writer = BufWriter::with_capacity(req.len(), server);
            // Write modified first line to buffer
            match buf_writer.write(first_line.as_bytes()).await {
                Ok(_) => (),
                Err(e) => {
                    logm(
                        Level::Info,
                        format!("error writing first line to server buffer: {}", e),
                    );
                    let _ = client_write.write(INTERNAL_SERVER_ERROR_RESPONSE).await;
                    return;
                }
            }
            let mut content_length = 0usize;
            let mut content_length_header: Vec<u8> = Vec::new();
            let mut iter = req.split_inclusive(|&b| b == b'\n');
            let mut header_size = iter.next().unwrap().len(); // There will always be a first line
            for line in iter {
                let mut got_content_length = false;
                header_size += line.len();
                if line.len() == 2 {
                    // Add the Forwarded header
                    let header = format!("Forwarded: for={}\r\n", client_addr.ip());
                    match buf_writer.write(header.as_bytes()).await {
                        Ok(_) => break,
                        #[allow(unused_must_use)]
                        Err(e) => {
                            logm(
                                Level::Info,
                                format!("error writing to server buffer: {}", e),
                            );
                            client_write.write(INTERNAL_SERVER_ERROR_RESPONSE).await;
                            return;
                        }
                    }
                }
                // Check if the first character is a 'c'
                // If so, it may be content-length, which we want to get
                // If there is no first, it's a malformed header; however, that will never
                // happen/the code will never be reached
                match line.get(0).map(u8::to_ascii_lowercase) {
                    Some(c) => {
                        if c == b'c' {
                            // Get the position of the colon to get the header name
                            let pos = match line.iter().position(|&b| b == b':') {
                                Some(pos) => pos,
                                #[allow(unused_must_use)]
                                None => {
                                    // NOTE: Send malformed header?
                                    client_write.write(BAD_REQUEST_RESPONSE).await;
                                    return;
                                }
                            };
                            // Convert the header name into a string
                            let mut header = match String::from_utf8(line.to_vec()) {
                                Ok(h) => h,
                                #[allow(unused_must_use)]
                                Err(_) => {
                                    // NOTE: Send malformed header?
                                    client_write.write(BAD_REQUEST_RESPONSE).await;
                                    return;
                                }
                            };
                            let (name, len) = header.split_at_mut(pos);
                            name.make_ascii_lowercase();
                            // Check if the header is content length; if so, parse the length
                            if name == "content-length" {
                                match (&len[1..]).trim().parse() {
                                    Ok(len) => {
                                        content_length = len;
                                        got_content_length = true;
                                    }
                                    #[allow(unused_must_use)]
                                    Err(_) => {
                                        // NOTE: Possibly ignore
                                        client_write.write(BAD_REQUEST_RESPONSE).await;
                                        return;
                                    }
                                }
                            }
                        }
                    }
                    #[allow(unused_must_use)]
                    None => {
                        client_write.write(BAD_REQUEST_RESPONSE).await;
                        return;
                    }
                }
                // Write the header to the buffer
                let fut = if got_content_length {
                    buf_writer.write(line)
                } else {
                    let _ = write!(
                        content_length_header,
                        "Content-Length: {}\r\n",
                        content_length
                    );
                    buf_writer.write(&content_length_header)
                };
                match fut.await {
                    Ok(_) => break,
                    #[allow(unused_must_use)]
                    Err(e) => {
                        logm(
                            Level::Info,
                            format!("error writing to server buffer: {}", e),
                        );
                        client_write.write(INTERNAL_SERVER_ERROR_RESPONSE).await;
                        return;
                    }
                }
            }
            // Write the buffer to the server stream
            match buf_writer.flush().await {
                Ok(_) => (),
                #[allow(unused_must_use)]
                Err(e) => {
                    logm(
                        Level::Info,
                        format!("error flushing buffer to server: {}", e),
                    );
                    client_write.write(INTERNAL_SERVER_ERROR_RESPONSE).await;
                    return;
                }
            }
            // Write body (or any extra headers)
            let mut server = buf_writer.into_inner();
            let total_size = header_size + content_length;
            if total_size >= BUFFER_SIZE {
                let mut diff = total_size - BUFFER_SIZE;
                // TODO: Handle better?
                loop {
                    // NOTE: 100 MS; make shorter?
                    match tokio::time::timeout(
                        Duration::from_millis(100),
                        client_read.read(&mut req_buf),
                    )
                    .await
                    {
                        Ok(res) => match res {
                            // NOTE: Handle 0 bytes?
                            Ok(l) => match server.write(&req_buf[..l]).await {
                                // NOTE: Handle different results of written bytes?
                                Ok(_) => {
                                    if diff < l {
                                        break;
                                    } else {
                                        diff -= l;
                                    }
                                }
                                Err(e) => {
                                    // TODO: Handle closed conn differently (don't log)
                                    logm(
                                        Level::Info,
                                        format!("error writing request to server stream: {}", e),
                                    );
                                    let _ =
                                        client_write.write(INTERNAL_SERVER_ERROR_RESPONSE).await;
                                    return;
                                }
                            },
                            Err(e) => {
                                // TODO: Handle closed conn differently (don't log)
                                logm(
                                    Level::Info,
                                    format!("error reading from client stream: {}", e),
                                );
                                return;
                            }
                        },
                        Err(_) => break,
                    }
                }
            } else if req.len() > total_size {
                match server.write(&req[header_size + 1..]).await {
                    // Add 1 to exclude last "\n"
                    // NOTE: Handle different results of written bytes?
                    Ok(_) => (),
                    Err(e) => {
                        logm(
                            Level::Info,
                            format!("error writing request to server: {}", e),
                        );
                        let _ = client_write.write(INTERNAL_SERVER_ERROR_RESPONSE).await;
                        return;
                    }
                }
            }
            bytes_read = 0; // Reset bytes read
            let (ref mut server_read, ref mut server_write) = tio::split(server);
            // Client always returns Either::Left and server Either::Right
            let client_fut = async {
                // Breaks Left(Ok(false)) if it should return
                loop {
                    match client_read.read(&mut req_buf).await {
                        Ok(0) => break Either::Left(Ok(0)), // NOTE: Ignore?
                        Ok(l) => {
                            match Self::parse_site_name(&req_buf[..l]).await {
                                Ok((name, _)) => {
                                    if name == site_name {
                                        break Either::Left(Ok(l));
                                    }
                                }
                                Err(_) => (),
                            }
                            match server_write.write(&req_buf[..l]).await {
                                Ok(_) => (),
                                Err(e) => break Either::Right(Err(e)),
                            }
                        }
                        Err(e) => break Either::Left(Err(e)),
                    }
                }
            };
            let server_fut = async {
                let mut buf = [0u8; BUFFER_SIZE];
                loop {
                    match server_read.read(&mut buf).await {
                        Ok(0) => break Either::Right(Ok(0)),
                        Ok(l) => match client_write.write(&buf[..l]).await {
                            Ok(_) => (),
                            Err(e) => break Either::Left(Err(e)),
                        },
                        Err(e) => break Either::Right(Err(e)),
                    }
                }
            };
            // Pipe the streams
            tokio::select! {
                lr = client_fut => {
                    match lr {
                        Either::Left(res) => match res {
                            Ok(0) => return,
                            Ok(l) => bytes_read = l,
                            Err(e) => {
                                logm(Level::Info, format!("error reading from client: {}", e));
                                return;
                            }
                        }
                        Either::Right(res) => match res {
                            Ok(l) => bytes_read = l, // Unreachable right now
                            Err(e) => {
                                logm(Level::Info, format!("error writing to server: {}", e));
                                let _ = client_write.write(INTERNAL_SERVER_ERROR_RESPONSE).await;
                            }
                        }
                    }
                }
                lr = server_fut => {
                    match lr {
                        Either::Left(res) => match res {
                            Err(e) => {
                                logm(Level::Info, format!("error writing to client: {}", e));
                            }
                            Ok(l) => bytes_read = l, // Unreachable right now
                        }
                        Either::Right(res) => match res {
                            Ok(l) => bytes_read = l,
                            Err(e) => {
                                logm(Level::Info, format!("error writing to server: {}", e));
                                let _ = client_write.write(INTERNAL_SERVER_ERROR_RESPONSE).await;
                            }
                        }
                    }
                }
            }
        }
    }

    async fn serve_home<W: AsyncWrite>(&self, _client: &mut WriteHalf<W>, _req: &[u8]) {
        // TODO
    }

    #[allow(unused_must_use)]
    async fn serve_favicon<W: AsyncWrite>(&self, client: &mut WriteHalf<W>, _req: &[u8]) {
        // TODO: Possibly handle error
        client.write(NOT_FOUND_RESPONSE).await;
    }

    async fn serve_tunnel<W: AsyncWrite>(&self, mut _client: WriteHalf<W>, _req: &[u8]) {
        // TODO
    }

    // Expects request without 2 byte header
    async fn serve_command<S: IOUnpin>(&self, mut _client: S, _req: &[u8]) {
        // TODO
    }

    // Returns the first slug of the path without the leading or trailing slashes and the first
    // line without the first slug in the path (includes "\r\n")
    // If there is an error, an HTTP error response is returned
    async fn parse_site_name(req: &[u8]) -> Result<(String, String), &'static [u8]> {
        let line_end_pos = match req.iter().position(|&b| b == b'\n') {
            Some(pos) => pos,
            None => return Err(BAD_REQUEST_RESPONSE),
        };
        let first_line = match String::from_utf8(req[..line_end_pos].to_vec()) {
            Ok(line) => line,
            Err(_) => return Err(BAD_REQUEST_RESPONSE),
        };
        let mut parts = req[..line_end_pos - 2].split(|&b| b == b' ');
        if parts.next().is_none() {
            return Err(BAD_REQUEST_RESPONSE);
        }
        if let Some(part) = parts.next() {
            if part.len() == 0 {
                return Err(BAD_REQUEST_RESPONSE);
            } else if part.len() == 1 {
                return Ok((String::from(""), first_line));
            } else {
                if let Some(pos) = part.iter().position(|&b| b == b'/' || b == b' ') {
                    match String::from_utf8(part[1..pos].to_vec()) {
                        Ok(name) => {
                            if part[pos] == b'/' {
                                Ok((name.clone(), first_line.replacen(&(name + "/"), "", 1)))
                            } else {
                                Ok((name.clone(), first_line.replacen(&name, "", 1)))
                            }
                        }
                        Err(_) => Err(BAD_REQUEST_RESPONSE),
                    }
                } else {
                    return Err(BAD_REQUEST_RESPONSE);
                }
            }
        } else {
            return Err(BAD_REQUEST_RESPONSE);
        }
        /*
        match REQ_RE.captures(req) {
            Some(caps) => match String::from_utf8(caps[1].to_vec()) {
                Ok(name) => Ok(name),
                Err(_) => Err(BAD_REQUEST_RESPONSE),
            },
            None => Err(BAD_REQUEST_RESPONSE),
        }
        */
    }

    // Expects request without 2 byte header
    fn shutdown(&self) {}

    fn force_shutdown(&self) {}
}

#[derive(Clone)]
struct ServerInfo {
    name: String,
    addr: String,
    tunnel: bool,
    secure: bool,
}

impl ServerInfo {
    fn new<S: ToString>(name: S, addr: S) -> Self {
        Self {
            name: name.to_string(),
            addr: addr.to_string(),
            tunnel: false,
            secure: false,
        }
    }
}

#[derive(PartialEq, Clone, Copy)]
enum ProxyStatus {
    NotStarted,
    Running,
    Stopping,
    Stopped,
}

// Source: https://users.rust-lang.org/t/how-to-nicely-match-a-number-to-an-enum/24494/7
// Source (Playground): https://play.rust-lang.org/?version=stable&mode=debug&edition=2018&gist=d2b8340564a41a04aaf629ae0d0e2584
macro_rules! match_enum_scalar {
    (match $val:ident as $tp:ty {
        $($pt:path => $bck:block),*
    }) => {
        match $val {
            $(val if val as $tp == $pt as $tp => Some($bck)),*
            _ => None
        }
    };
    (match $val:ident as $tp:ty {
        $($pt:path => $bck:expr),*
    }) => {
        match $val {
            $(val if val as $tp == $pt as $tp => Some($bck)),*,
            _ => None
        }
    };
}

#[repr(u16)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum SpecialReqHeader {
    Unknown = 0,
    Command = 3375,
    Tunnel = 31623,
}

impl SpecialReqHeader {
    const fn is_tunnel(u: u16) -> bool {
        Self::Tunnel as u16 == u
    }

    const fn is_command(u: u16) -> bool {
        Self::Command as u16 == u
    }

    const fn to_bytes(self) -> [u8; 2] {
        to16(self as u16)
    }
}

impl From<u16> for SpecialReqHeader {
    fn from(u: u16) -> Self {
        match_enum_scalar! {
            match u as u16 {
                Self::Command => Self::Command,
                Self::Tunnel => Self::Tunnel,
                Self::Unknown => Self::Unknown
            }
        }
        .unwrap_or(Self::Unknown)
    }
}

impl From<&[u8]> for SpecialReqHeader {
    fn from(bytes: &[u8]) -> Self {
        Self::from(from16(bytes))
    }
}

fn from16(bytes: &[u8]) -> u16 {
    (bytes[1] as u16) << 8 | (bytes[0] as u16)
}

const fn to16(n: u16) -> [u8; 2] {
    [(n >> 8) as u8, n as u8]
}
