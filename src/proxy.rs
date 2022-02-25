#![allow(dead_code)]
use log::{log, Level};
use regex::bytes::Regex;
use socket2::{Domain, Protocol, Socket, Type};
use std::collections::HashMap;
use std::io;
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::{
    atomic::{AtomicBool, AtomicU32, Ordering},
    Arc,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::rustls;
use tokio_rustls::TlsAcceptor;

const REQUEST_BUFFER_SIZE: usize = 1 << 10;

lazy_static! {
    static ref REQ_RE: Regex = Regex::new(r"^\w+ /([^/ ]+)?(/.+)? HTTP").unwrap();
}

#[derive(Clone)]
pub struct Proxy {
    //acceptor: TlsAcceptor,
    acceptor: Option<TlsAcceptor>,
    addr: SocketAddr,
    servers: Arc<HashMap<String, ServerInfo>>,
    shutdown: Arc<AtomicBool>,
    force_shutdown: Arc<AtomicBool>,
    status: Arc<ProxyStatus>,
    num_running: Arc<AtomicU32>,
    server_info_map: Arc<HashMap<String, ServerInfo>>,
    tunnel_reqs: Arc<AtomicU32>,
    reserved_paths: Arc<HashMap<String, Box<dyn Fn()>>>,
}

impl Proxy {
    pub fn new<A: ToSocketAddrs>(addr: A) -> io::Result<Self> {
        let addr = addr
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| io::Error::from(io::ErrorKind::AddrNotAvailable))?;
        Ok(Self {
            //acceptor: TlsAcceptor::from(Arc::new(rustls::ServerConfig::builder().with_safe_defaults().with_no_client_auth())),
            acceptor: None,
            addr: addr,
            servers: Arc::new(HashMap::new()),
            shutdown: Arc::new(AtomicBool::new(false)),
            force_shutdown: Arc::new(AtomicBool::new(false)),
            status: Arc::new(ProxyStatus::NotStarted),
            num_running: Arc::new(AtomicU32::new(0)),
            server_info_map: Arc::new(HashMap::new()),
            tunnel_reqs: Arc::new(AtomicU32::new(0)),
            reserved_paths: Arc::new(HashMap::new()),
        })
    }

    pub async fn run(&self) -> io::Result<()> {
        let socket = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP))?;
        socket.set_reuse_port(true)?;
        //let addr = self.addr.clone().into();
        //socket.bind(&addr)?;
        // TODO: Possibly use socket.listen function
        socket.set_nonblocking(true)?;
        let ln = TcpListener::from_std(socket.into())?;
        loop {
            // TODO: Poll accept while watching proxy status; possibly use oneshot channel
            match ln.accept().await {
                /*
                Ok((conn, addr)) => tokio::spawn(async move {
                    self.accept(conn, addr),
                });
                // */
                Ok(_) => (),
                Err(err) => eprintln!("{}", err), // TODO: Handle different errors
            }
        }
        println!("running");
        Ok(())
    }

    fn accept(self, conn: TcpStream, addr: SocketAddr) {
        if let Some(acceptor) = self.acceptor.as_ref() {
            let mut stream = acceptor.accept(conn);
            self.handle();
        } else {
            self.handle();
        }
    }

    fn handle<S: AsyncRead + AsyncWrite>(self, mut client: S, client_addr: SocketAddr) {
        // TODO: Create log function
        let client_addr_clone = client_addr.clone();
        let logm = |level, msg| {
            log!(level, "client: {}, server: N/A: {}", client_addr_clone, msg);
        };
        let mut req_buf = [0u8; REQUEST_BUFFER_SIZE];
        // Read from the stream with the timeout
        let bytes_read = match tokio::time::timeout(
            Duration::from_millis(STREAM_READ_TIMEOUT),
            client.read(&mut req_buf),
        ) {
            Ok(res) => match res {
                Ok(l) if l < 2 => {
                    #[allow(unused_must_use)]
                    {
                        client.write(BAD_REQUEST_RESPONSE).await;
                    }
                    return;
                }
                Ok(l) => l,
                Err(e) => {
                    // TODO: Log
                    eprintln!("error reading from client stream: {}", e);
                    return;
                }
            },
            Err(e) => {
                // TODO: Log
                eprintln!("client stream read timed out");
                return;
            }
        };
        // Check if the request is a special request
        match ReqHeader::from(&req_buf[..2]) {
            ReqHeader::Tunnel => (),  // TODO: Handle tunnel
            ReqHeader::Command => (), // TODO: Handle command
            ReqHeader::Unknown => (),
        }
        let req = &req_buf[..bytes_read];
        // TODO: Use REQ_RE to parse first line of request
    }

    async fn pipe_conns<T, U>(&self, reader: ReadHalf<T>, writer: WriteHalf<U>) {
        //
    }

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
                Self::Unknown => Self::Unknown,
            }
        }
        .unwrap_or(Self::Uknown)
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
