// TODO: Also create parse_requset function that parses and reformats a request
// that takes an optional server name, checking if the request is asking for
// the same server, if not, connecting to the new server
// TODO: Requests with no server name (i.e., no path) cause a thread panic from
// regex becuase there is no group index at 1
// TODO: Take logm closures in all "serve_" functions
#![allow(dead_code)]
#![allow(unused_imports)]
#![allow(unused_variables)]
use either::Either;
use lazy_static::lazy_static;
use log::{log, Level};
use regex::bytes::Regex; // TODO: Possibly use regular str version
use socket2::{Domain, Protocol, Socket, Type};
use std::io;
use std::marker::Unpin;
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::{
    atomic::{AtomicBool, AtomicU32, Ordering},
    Arc,
};
use std::time::Duration;
use tokio::io::{
    self as tio, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader,
    ReadHalf, WriteHalf,
};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::rustls;
use tokio_rustls::{TlsAcceptor, TlsConnector};

use rusty_proxy::sync_map::SyncMap;

const BUFFER_SIZE: usize = 1 << 10;
const STREAM_READ_TIMEOUT: u64 = 10_000; // In ms
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

    async fn handle<S: IOUnpin>(self, client: S, client_addr: SocketAddr) {
        // TODO: Create log function
        let client_addr_clone = client_addr.clone();
        let logm = |level, msg| {
            log!(level, "client: {}, server: N/A: {}", client_addr_clone, msg);
        };
        let (client_read, mut client_write) = tio::split(client);
        //let mut req_buf = [0u8; BUFFER_SIZE];
        let mut client_reader = BufReader::with_capacity(BUFFER_SIZE, client_read);
        // Read from the stream with the timeout
        let mut first_line_buf = Vec::new();
        let bytes_read = match tokio::time::timeout(
            Duration::from_millis(STREAM_READ_TIMEOUT),
            client_reader.read_until(b'\n', &mut first_line_buf),
            //client.read(&mut req_buf),
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
        // Check if the request is a special request
        match ReqHeader::from(&first_line_buf[..2]) {
            ReqHeader::Tunnel => (),  // TODO: Handle tunnel
            ReqHeader::Command => (), // TODO: Handle command
            ReqHeader::Unknown => (),
        }
        //let req = req_buf[..bytes_read];
        let req = first_line_buf.as_slice();
        let site_name = match REQ_RE.captures(req) {
            Some(caps) => match String::from_utf8(caps[1].to_vec()) {
                Ok(name) => name,
                #[allow(unused_must_use)] // Used for the write below
                Err(_) => {
                    client_write.write(BAD_REQUEST_RESPONSE).await;
                    return;
                }
            },
            #[allow(unused_must_use)] // Used for the write below
            None => {
                client_write.write(BAD_REQUEST_RESPONSE).await;
                return;
            }
        };
        // Check if the site is reserved
        match site_name.as_str() {
            "" => {
                let client_read = client_reader.into_inner();
                self.serve_home(client_read.unsplit(client_write), req)
                    .await;
                return;
            }
            "favicon.ico" => {
                let client_read = client_reader.into_inner();
                self.serve_favicon(client_read.unsplit(client_write), req)
                    .await;
                return;
            }
            _ => (),
        }
        // Remove the site name from the request URI
        // TODO: Find better way to do this
        let req_str = String::from_utf8(req.to_vec()).unwrap();
        let index = req_str.find('/').unwrap() + site_name.len() + 1;
        let req_str = if req_str.get(index..index).unwrap() == "/" {
            req_str.replacen(&(site_name.clone() + "/"), "", 1)
        } else {
            req_str.replacen(&site_name, "", 1)
        };
        let header = format!("\r\nForwarded: for={}\r\n", client_addr.ip());
        // Don't have to use replacen since the req only goes to the first '\n'
        let req_str = req_str.replacen("\r\n", &header, 1);
        // TODO: Switch to sync map methods
        // Get the server info
        let server_info = if let Some(info) = self.server_info_map.load(&site_name) {
            info
        } else {
            #[allow(unused_must_use)]
            {
                client_write.write(NOT_FOUND_RESPONSE).await;
            }
            return;
        };
        let server_addr_clone = server_info.addr.clone();
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
        let server_stream = match TcpStream::connect(&server_info.addr).await {
            Ok(stream) => stream,
            #[allow(unused_must_use)]
            Err(e) => {
                client_write.write(INTERNAL_SERVER_ERROR_RESPONSE).await;
                return;
            }
        };
        let mut server_stream: Box<dyn IOUnpin> = if server_info.secure {
            // TODO:
            if let Some(connector) = self.connector.clone() {
                // TODO: Connect
                Box::new(server_stream)
            } else {
                // TODO: Log error (error is that there needs to be a client connector
                return;
            }
        } else {
            Box::new(server_stream)
        };
        // TODO: Do better
        // Write the request to the server
        match server_stream.write(req_str.as_bytes()).await {
            Ok(_) => (),
            #[allow(unused_must_use)]
            Err(e) => {
                logm(
                    Level::Info,
                    format!("error writing request to server: {}", e),
                );
                client_write.write(INTERNAL_SERVER_ERROR_RESPONSE).await;
                return;
            }
        }
        match server_stream.write(client_reader.buffer()).await {
            Ok(_) => (),
            #[allow(unused_must_use)]
            Err(e) => {
                logm(
                    Level::Info,
                    format!("error writing request to server: {}", e),
                );
                client_write.write(INTERNAL_SERVER_ERROR_RESPONSE).await;
                return;
            }
        }
        let client_stream = client_reader.into_inner().unsplit(client_write);
        let (ref mut cr, ref mut cw) = tio::split(client_stream);
        let (ref mut sr, ref mut sw) = tio::split(server_stream);
        loop {
            tokio::select! {
                _ = self.pipe_conns(cr, sw) => (),
                _ = self.pipe_conns(sr, cw) => (),
            }
        }
    }

    // read_half error returned as left, write_half error returned as right
    async fn pipe_conns<R, W>(&self, read_half: &mut ReadHalf<R>, write_half: &mut WriteHalf<W>) -> Either<io::Result<()>, io::Result<()>>
    where
        R: AsyncRead,
        W: AsyncWrite,
    {
        // TODO: Handle/Return error
        let mut buffer = [0u8; BUFFER_SIZE];
        loop {
            match read_half.read(&mut buffer).await {
                Ok(0) => return Either::Left(Ok(())),
                Ok(size) => match write_half.write(&buffer[..size]).await {
                    Ok(_) => (),
                    Err(e) => return Either::Right(Err(e)),
                },
                Err(e) => return Either::Left(Err(e)),
            }
        }
    }

    async fn serve_home<S: IOUnpin>(&self, mut client: S, req: &[u8]) {
        // TODO
    }

    #[allow(unused_must_use)]
    async fn serve_favicon<S: IOUnpin>(&self, mut client: S, _: &[u8]) {
        // TODO: Possibly handle error
        client.write(NOT_FOUND_RESPONSE).await;
    }

    async fn serve_tunnel<S: IOUnpin>(&self, mut clinet: S, req: &[u8]) {
        // TODO
    }

    // Expects request without 2 byte header
    async fn serve_command<S: IOUnpin>(&self, mut clinet: S, req: &[u8]) {
        // TODO
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
enum ReqHeader {
    Unknown = 0,
    Command = 3375,
    Tunnel = 31623,
}

impl ReqHeader {
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

impl From<u16> for ReqHeader {
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

impl From<&[u8]> for ReqHeader {
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
