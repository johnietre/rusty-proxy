#![allow(dead_code)]
//#![allow(unused_imports)]
#![allow(unused_variables)]
use log::{log, Level};
use rusty_proxy::sync_map::SyncMap;
use serde::{Deserialize, Serialize};
use socket2::{Domain, Protocol, Socket, Type};
use std::io;
use std::marker::Unpin;
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::io::{
    self as tio, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufStream, BufWriter,
};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::{TlsAcceptor, TlsConnector};

const BUFFER_SIZE: usize = 1 << 10;
const STREAM_READ_TIMEOUT: u64 = 10_000; // In ms
const TUNNEL_QUEUE_LEN: usize = 1_000;

// HTTP Responses
const OK_RESPONSE: &[u8] = b"HTTP/1.1 200 OK\r\n\r\n";
const BAD_REQUEST_RESPONSE: &[u8] = b"HTTP/1.1 400 Bad Request\r\n\r\n";
const NOT_FOUND_RESPONSE: &[u8] = b"HTTP/1.1 404 Not Found\r\n\r\n";
const METHOD_NOT_ALLOWED_RESPONSE: &[u8] = b"HTTP/1.1 405 Method Not Allowed\r\n\r\n";
const INTERNAL_SERVER_ERROR_RESPONSE: &[u8] = b"HTTP/1.1 500 Internal Server Error\r\n\r\n";
const NOT_IMPLEMENTED_RESPONSE: &[u8] = b"HTTP/1.1 501 Not Implemented\r\n\r\n";

// The HTML page to be served
const HOME_HTML: &str = r#"
<!DOCTYPE html>
<html lang="en-US">
<head>
<title>Rusty Proxy</title><meta charset="UTF-8">
<meta name="viewport content="width=device-width, initial-scale=1.0"></head>
<body>%REPLACE%</body>
</html>
"#;

trait IORead: AsyncRead + Unpin + Send + Sync {}
impl<T> IORead for T where T: AsyncRead + Unpin + Send + Sync {}

trait IOWrite: AsyncWrite + Unpin + Send + Sync {}
impl<T> IOWrite for T where T: AsyncWrite + Unpin + Send + Sync {}

trait IORW: IOWrite + IORead {}
impl<T> IORW for T where T: IOWrite + IORead {}

#[derive(Clone)]
pub struct Proxy {
    // Stuff for TLS
    acceptor: Option<TlsAcceptor>,
    connector: Option<TlsConnector>,

    // Address the proxy is running on
    addr: SocketAddr,
    routes: SyncMap<Server>,
    // The bytes of the HTML home page
    // Should be updated after a route is added/deleted
    home_html_bytes: Arc<RwLock<Vec<u8>>>,
    /*
    // Stuff for tunnels connecting
    // The queue of tunnel conns
    tunnel_queue: Arc<[u8]>,
    // The current tunnel id
    tunnel_id: Arc<AtomicUsize>,

    // Stuff for tunneling
    // Address of the proxy tunneling to (redundant?)
    tunnel_addr: Arc<String>,
    // The connection to the tunnel
    tunnel_conn: Option<Arc<dyn IOUnpin>>,
    // The information about this proxy tunneling to?
    tunnel_server: Option<Arc<Server>>,
    */
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
            addr,
            routes: SyncMap::new(),
            home_html_bytes: Arc::new(RwLock::new(HOME_HTML.replace("%REPLACE%", "").into_bytes())),
            /*
                    tunnel_queue: Arc::new([0u8; TUNNEL_QUEUE_LEN]),
                    tunnel_id: Arc::new(AtomicUsize::new(0)),
                    tunnel_addr: Arc::new(String::new()),
                    tunnel_conn: None,
                    tunnel_server: None,
            */
        })
    }

    pub async fn run(&self) -> io::Result<()> {
        let socket = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP))?;
        #[cfg(target = "unix")]
        {
            socket.set_reuse_port(true)?;
        }
        let addr = self.addr.clone().into();
        socket.bind(&addr)?;
        socket.listen(128)?;
        socket.set_nonblocking(true)?;
        let ln = TcpListener::from_std(socket.into())?;
        log!(Level::Trace, "listening on {}", addr.as_socket().unwrap());
        loop {
            match ln.accept().await {
                Ok((conn, addr)) => {
                    let px = self.clone();
                    tokio::spawn(async move {
                        px.accept(conn, addr).await;
                    });
                }
                Err(err) => break Err(err),
            }
        }
    }

    async fn accept(self, client: TcpStream, addr: SocketAddr) {
        let addr_clone = addr.clone();
        let logm = |lvl, msg| {
            log!(lvl, "client: {}, server: N/A: {}", addr_clone, msg);
        };
        if let Some(acceptor) = self.acceptor.as_ref() {
            match acceptor.accept(client).await {
                Ok(client) => self.handle(client, addr, logm).await,
                Err(e) => logm(Level::Trace, format!("error accepting client TLS: {}", e)),
            }
        } else {
            self.handle(client, addr, logm).await;
        }
    }

    async fn handle<F, S>(self, client: S, client_addr: SocketAddr, logm: F)
    where
        F: Fn(Level, String),
        //S: IOUnpin,
        S: IORW,
    {
        use SpecialReqHeader::*;
        // Create the buffered stream
        let mut buf_stream = BufStream::new(client);
        // Read the header
        let mut buf = [0; 4];
        match tokio::time::timeout(
            Duration::from_millis(STREAM_READ_TIMEOUT),
            buf_stream.read(&mut buf),
        )
        .await
        {
            Ok(res) => match res {
                Ok(n) => n,
                Err(e) => {
                    logm(Level::Info, format!("error reading from client: {}", e));
                    return;
                }
            },
            Err(_) => {
                logm(Level::Trace, String::from("client stream read timed out"));
                return;
            }
        };
        // Check for header
        match SpecialReqHeader::from(&buf[..]) {
            Connect => self.handle_connect(buf_stream, client_addr, logm).await,
            Tunnel => self.handle_tunnel(buf_stream, client_addr, logm).await,
            _ => self.handle_http(buf_stream, client_addr, logm, &buf).await,
        }
    }

    async fn handle_http<F, S>(
        self,
        buf_client: BufStream<S>,
        client_addr: SocketAddr,
        logm: F,
        first_bytes: &[u8],
    ) where
        //S: IOUnpin,
        S: IORW,
        F: Fn(Level, String),
    {
        // Split the client
        let (ref mut client_read, ref mut client_write) = tio::split(buf_client);
        // Create the request buffer with the initial bytes previously read
        let mut req_buf = [0u8; BUFFER_SIZE];
        (&mut req_buf[..first_bytes.len()]).copy_from_slice(first_bytes);
        // Read from the client
        let mut bytes_read = match tokio::time::timeout(
            Duration::from_millis(STREAM_READ_TIMEOUT),
            client_read.read(&mut req_buf[first_bytes.len()..]),
        )
        .await
        {
            Ok(res) => match res {
                Ok(n) => n,
                Err(e) => {
                    logm(Level::Info, format!("error reading from client: {}", e));
                    return;
                }
            },
            Err(_) => {
                logm(Level::Trace, String::from("client stream read timed out"));
                return;
            }
        } + first_bytes.len();

        loop {
            // Read a request from the client if needed
            // In order to read a new request on a new loop iteration, 'bytes_read' needs to be
            // reset to 0 prior to the new iteration
            if bytes_read == 0 {
                bytes_read = match tokio::time::timeout(
                    Duration::from_millis(STREAM_READ_TIMEOUT),
                    client_read.read(&mut req_buf),
                )
                .await
                {
                    Ok(res) => match res {
                        Ok(n) => n,
                        Err(e) => {
                            logm(Level::Info, format!("error reading from client: {}", e));
                            return;
                        }
                    },
                    Err(_) => {
                        logm(Level::Trace, String::from("client stream read timed out"));
                        return;
                    }
                }
            }
            // Get the site name and the new first line
            let (site_name, first_line) = match Self::remove_site_name(&req_buf[..bytes_read]) {
                Ok(parts) => parts,
                Err(e) => {
                    Self::write_flush_ignored(client_write, e).await;
                    return;
                }
            };
            // Check for special headers
            match site_name.as_str() {
                "" => {
                    self.serve_base(client_write, &req_buf[..bytes_read]).await;
                    bytes_read = 0;
                    continue;
                }
                "favicon.ico" => {
                    self.serve_favicon(client_write, &req_buf[..bytes_read])
                        .await;
                    bytes_read = 0;
                    continue;
                }
                _ => (),
            }
            // Get the server or send not found
            // NOTE: Possiby redirect to home on not found
            let server_info = if let Some(info) = self.routes.load(&site_name) {
                info
            } else {
                Self::write_flush_ignored(client_write, NOT_FOUND_RESPONSE).await;
                bytes_read = 0;
                continue;
            };
            // Create the 'logm' closure
            let client_addr_clone = client_addr.clone();
            let server_addr_clone = server_info.addr.clone();
            let logm = |lvl, msg| {
                log!(
                    lvl,
                    "client: {}, server: {}: {}",
                    client_addr_clone,
                    server_addr_clone,
                    msg
                );
            };
            // TODO: Handle tunnel
            // Connect to the server
            let server = match TcpStream::connect(&server_info.addr).await {
                Ok(stream) => stream,
                Err(e) => {
                    logm(Level::Info, format!("error connecting to server: {}", e));
                    Self::write_flush_ignored(client_write, INTERNAL_SERVER_ERROR_RESPONSE).await;
                    bytes_read = 0;
                    continue;
                }
            };
            let server: Box<dyn IORW> = if server_info.secure {
                if let Some(_connector) = self.connector.clone() {
                    // TODO
                    Box::new(server)
                } else {
                    logm(Level::Info, format!("missing TLS connector"));
                    Self::write_flush_ignored(client_write, INTERNAL_SERVER_ERROR_RESPONSE).await;
                    bytes_read = 0;
                    continue;
                }
            } else {
                Box::new(server)
            };
            // Split the server
            let (ref mut server_read, server_write) = tio::split(server);
            let ref mut server_write = BufWriter::new(server_write);
            // Send the request to the server
            match Self::write_req(
                server_write,
                &req_buf[..bytes_read],
                &site_name,
                &first_line,
            )
            .await
            {
                Ok(_) => (),
                Err(e) => {
                    logm(Level::Info, format!("error writing to server: {}", e));
                    Self::write_flush_ignored(client_write, INTERNAL_SERVER_ERROR_RESPONSE).await;
                    bytes_read = 0;
                    continue;
                }
            }
            let (ico_tx, mut ico_rx) = tokio::sync::mpsc::channel(5);
            // Create the client future to read from the client and write to the server
            let client_fut = async {
                loop {
                    match client_read.read(&mut req_buf).await {
                        Ok(0) => break Either::Left(Ok(0)), // NOTE: Ignore?
                        // Parse the site name and clean the first line
                        Ok(l) => match Self::remove_site_name(&req_buf[..l]) {
                            Ok((name, line)) => {
                                if name == "favicon.ico" {
                                    let _ = ico_tx.send(()).await; // Ignore error
                                    continue;
                                }
                                // Break out the loop if there's a different site name
                                if name != site_name {
                                    break Either::Left(Ok(l));
                                }
                                // Write the request to the server
                                match Self::write_req(server_write, &req_buf[..l], &name, &line)
                                    .await
                                {
                                    Ok(_) => (),
                                    Err(e) => break Either::Right(Some(e)),
                                }
                            }
                            // Ignore errors
                            // Write the request to the server
                            Err(_) => match Self::write_flush(server_write, &req_buf[..l]).await {
                                Ok(_) => (),
                                Err(e) => break Either::Right(Some(e)),
                            },
                        },
                        Err(e) => break Either::Left(Err(e)),
                    }
                }
            };
            // Create the server future to read from the server and write to the client
            let server_fut = async {
                let mut buf = [0u8; BUFFER_SIZE];
                loop {
                    // Listen eithr for a server message or a request to send favicon.ico
                    let res = tokio::select! {
                        res = server_read.read(&mut buf) => res,
                        _ = ico_rx.recv() => {
                            match Self::write_flush(client_write, NOT_FOUND_RESPONSE).await {
                                Ok(_) => continue,
                                Err(e) => break Either::Left(Err(e)),
                            }
                        }
                    };
                    //match server_read.read(&mut buf).await
                    match res {
                        Ok(l) => match Self::write_flush(client_write, &buf[..l]).await {
                            Ok(_) => (),
                            Err(e) => break Either::Left(Err(e)),
                        },
                        Err(e) => break Either::Right(Some(e)),
                    }
                }
            };
            // Pipe the streams
            // Anything dealing with the client is returned as 'Left' and anything dealing with
            // the server is returned as 'Right'
            // Since the result of a server can only be an error, an "Option" containing the error
            // is returned rather than a "Result"
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
                            Some(e) => {
                                logm(
                                    Level::Info,
                                    format!("error writing to server: {}", e)
                                );
                                bytes_read = 0;
                                Self::write_flush_ignored(
                                    client_write,
                                    INTERNAL_SERVER_ERROR_RESPONSE,
                                ).await;
                            }
                            None => unreachable!(),
                        }
                    }
                }
                lr = server_fut => {
                    match lr {
                        Either::Left(res) => match res {
                            Err(e) => {
                                logm(Level::Info, format!("error writing to client: {}", e));
                            }
                            Ok(l) => bytes_read = l,
                        }
                        Either::Right(res) => match res {
                            Some(e) => {
                                logm(Level::Info, format!("error writing to server: {}", e));
                                bytes_read = 0;
                                Self::write_flush_ignored(
                                    client_write,
                                    INTERNAL_SERVER_ERROR_RESPONSE,
                                ).await;
                            }
                            None => unreachable!(),
                        }
                    }
                }
            }
        }
    }

    async fn serve_base(&self, client_write: &mut impl IOWrite, req: &[u8]) {
        let method = req
            .iter()
            .map_while(|&b| {
                if b != b' ' {
                    Some(b.to_ascii_uppercase())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        match method.as_slice() {
            b"GET" => self.serve_home_page(client_write, req).await,
            b"POST" => self.add_server(client_write, req).await,
            b"DELETE" => self.delete_server(client_write, req).await,
            _ => Self::write_flush_ignored(client_write, METHOD_NOT_ALLOWED_RESPONSE).await,
        }
    }

    async fn serve_home_page(&self, client_write: &mut impl IOWrite, _req: &[u8]) {
        let html = self.home_html_bytes.read().unwrap().clone();
        Self::write_flush_ignored(client_write, &html).await;
    }

    async fn add_server(&self, client_write: &mut impl IOWrite, req: &[u8]) {
        let server = match Self::find_body_start(req) {
            Some(i) => match serde_json::from_slice::<Server>(&req[i..]) {
                Ok(srvr) if srvr.path != "" && srvr.addr != "" && srvr.name != "" => srvr,
                _ => {
                    Self::write_flush_ignored(client_write, BAD_REQUEST_RESPONSE).await;
                    return;
                }
            },
            None => {
                Self::write_flush_ignored(client_write, BAD_REQUEST_RESPONSE).await;
                return;
            }
        };
        //self.routes.load_or_store;
    }

    // NOTE: It's possible for a delete-add-(accidental) delete
    async fn delete_server(&self, client_write: &mut impl IOWrite, req: &[u8]) {
        let server = match Self::find_body_start(req) {
            Some(i) => match serde_json::from_slice::<Server>(&req[i..]) {
                Ok(srvr) if srvr.path != "" && srvr.addr != "" && srvr.name != "" => srvr,
                _ => {
                    Self::write_flush_ignored(client_write, BAD_REQUEST_RESPONSE).await;
                    return;
                }
            },
            None => {
                Self::write_flush_ignored(client_write, BAD_REQUEST_RESPONSE).await;
                return;
            }
        };
        // Load the info of the server sent
        let srvr = if let Some(srvr) = self.routes.load(&server.path) {
            srvr
        } else {
            Self::write_flush_ignored(client_write, NOT_FOUND_RESPONSE).await;
            return;
        };
        // Check the infos
        if srvr.name != server.name || srvr.path != server.path || srvr.addr != server.addr {
            Self::write_flush_ignored(client_write, NOT_FOUND_RESPONSE).await;
            return;
        }
        // Delete the server
        self.routes.delete(&server.path);
        // TODO: Update HTML
        Self::write_flush_ignored(client_write, OK_RESPONSE).await;
    }

    async fn serve_favicon(&self, client_write: &mut impl IOWrite, _req: &[u8]) {
        Self::write_flush_ignored(client_write, NOT_FOUND_RESPONSE).await;
    }

    async fn write_req(
        server_write: &mut impl IOWrite,
        req: &[u8],
        site_name: &str,
        first_line: &str,
    ) -> io::Result<()> {
        let mut line_ends =
            req.iter()
                .enumerate()
                .filter_map(|(i, &b)| if b == b'\n' { Some(i + 1) } else { None });
        server_write.write(first_line.as_bytes()).await?;
        // The start of the next line
        // Skip the first line; don't worry about if 'next()' is 'None' since if it's 'None', the
        // loop won't be run so there's nothing to worry about
        let mut start = line_ends.next().unwrap_or(0);
        // Keep track of whether the proxy header was added
        let mut added_header = false;
        for end in line_ends {
            // Check to see if we reached the end of the headers (line of only "\r\n")
            if end - start == 2 {
                if !added_header {
                    server_write
                        .write(format!("Rusty-Proxy-Path: {}\r\n\r\n", site_name).as_bytes())
                        .await?;
                } else {
                    server_write.write(b"\r\n").await?;
                }
                start = end;
                break;
            }
            let line = &req[start..end];
            if !added_header && Self::is_http_header(line, "rusty-proxy-path".as_bytes()) {
                let header = String::from_utf8_lossy(&line[..end - 2]);
                server_write
                    .write(format!("{}, {}\r\n", header, site_name).as_bytes())
                    .await?;
                added_header = true;
            } else {
                server_write.write(&req[start..end]).await?;
            }
            start = end;
        }
        // Send the rest of the request passed
        server_write.write(&req[start..]).await?;
        server_write.flush().await
    }

    // Returns the index of the first byte of the body
    fn find_body_start(req: &[u8]) -> Option<usize> {
        let line_ends =
            req.iter()
                .enumerate()
                .filter_map(|(i, &b)| if b == b'\n' { Some(i + 1) } else { None });
        let mut start = 0;
        for end in line_ends {
            if &req[start..end] == b"\r\n" {
                // End of the request (no body)
                if end == req.len() {
                    return None;
                }
                return Some(end);
            }
            start = end;
        }
        None
    }

    fn is_http_header(have: &[u8], want: &[u8]) -> bool {
        have.len() > want.len()
            && have
                .iter()
                .zip(want)
                .all(|(h, w)| h.to_ascii_uppercase() == w.to_ascii_uppercase())
            && have[want.len()] == b':'
    }

    // Returns the first slug of the path without leading or trailing slashes and the first line
    // of the with the slug removed (includes the new line character)
    // On error, it returns an error response to be sent to the client
    fn remove_site_name(req: &[u8]) -> Result<(String, String), &'static [u8]> {
        let (i1, i2) = Self::parse_site_name_indices(req)?;
        let line_end = req
            .iter()
            .position(|&b| b == b'\n')
            .ok_or(BAD_REQUEST_RESPONSE)?
            + 1;
        let mut line =
            String::from_utf8((&req[..line_end]).to_vec()).or(Err(BAD_REQUEST_RESPONSE))?;
        if req[i2] == b'/' {
            line.replace_range(i1..i2 + 1, "");
        } else {
            line.replace_range(i1..i2, "");
        }
        String::from_utf8((&req[i1..i2]).to_vec())
            .or(Err(BAD_REQUEST_RESPONSE))
            .map(|name| (name, line))
    }

    // Returns the indices of the start and end of the first slug in the request url without any
    // leading or trailing slashed.
    // On error, it returns an error response to be sent to the client
    fn parse_site_name_indices(req: &[u8]) -> Result<(usize, usize), &'static [u8]> {
        let mut i1 = req
            .iter()
            .position(|&b| b == b' ')
            .ok_or(BAD_REQUEST_RESPONSE)?
            + 1;
        let i2 = req
            .iter()
            .skip(i1)
            .position(|&b| b == b' ')
            .ok_or(BAD_REQUEST_RESPONSE)?
            + i1;
        if req[i1] == b'/' {
            i1 += 1;
        }
        let i2 = if let Some(i2) = (&req[i1..i2]).iter().position(|&b| b == b'/') {
            i2 + i1
        } else {
            i2
        };
        Ok((i1, i2))
    }

    async fn handle_connect<F, S>(self, buf_client: BufStream<S>, client_addr: SocketAddr, logm: F)
    where
        S: IORW,
        F: Fn(Level, String),
    {
        println!("connect");
    }

    async fn handle_tunnel<F, S>(self, buf_stream: BufStream<S>, client_addr: SocketAddr, logm: F)
    where
        S: IORW,
        F: Fn(Level, String),
    {
        println!("tunnel");
    }

    async fn write_flush(stream: &mut impl IOWrite, b: &[u8]) -> io::Result<()> {
        stream.write(b).await?;
        stream.flush().await
    }

    #[allow(unused_must_use)]
    async fn write_flush_ignored(stream: &mut impl IOWrite, b: &[u8]) {
        stream.write(b).await;
        stream.flush().await;
    }
}

#[derive(Default, Serialize, Deserialize)]
struct Server {
    name: String,
    path: String,
    addr: String,
    hidden: bool,

    #[serde(skip)]
    secure: bool,
    #[serde(skip)]
    tunnel: Option<Arc<dyn IORW>>,
}

impl Server {
    fn new() -> Self {
        Self::default()
    }

    fn name<S: ToString>(self, name: S) -> Self {
        Self {
            name: name.to_string(),
            ..self
        }
    }

    fn path<S: ToString>(self, path: S) -> Self {
        Self {
            path: path.to_string(),
            ..self
        }
    }

    fn hidden(self, hidden: bool) -> Self {
        Self { hidden, ..self }
    }

    fn secure(self, secure: bool) -> Self {
        Self { secure, ..self }
    }

    /*
    fn tunnel(self, tunnel: Option<Arc<Mutex<dyn IORW>>>) -> Self {
        Self { tunnel, ..self }
    }
    */

    fn to_link(&self) -> String {
        if self.hidden {
            String::new()
        } else {
            format!(r#"<a href="./{}">{}</a>"#, self.path, self.name)
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

#[repr(u32)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum SpecialReqHeader {
    Nothing = 0x0,
    Tunnel = 0xFFFFFFFF,
    Connect = 0xFFFFFFFE,
    Success = 0xFFFFFFFD,
    BadMessage = 0xFFFFFFFC,
    AlreadyExists = 0xFFFFFFFB,
}

impl SpecialReqHeader {}

impl From<u32> for SpecialReqHeader {
    fn from(u: u32) -> Self {
        match_enum_scalar! {
            match u as u32 {
                Self::Nothing => Self::Nothing,
                Self::Tunnel => Self::Tunnel,
                Self::Connect => Self::Connect,
                Self::Success => Self::Success,
                Self::BadMessage => Self::BadMessage,
                Self::AlreadyExists => Self::AlreadyExists
            }
        }
        .unwrap_or(Self::Nothing)
    }
}

impl From<&[u8]> for SpecialReqHeader {
    fn from(bytes: &[u8]) -> Self {
        if bytes.len() < 4 {
            return Self::Nothing;
        }
        Self::from(from32(bytes))
    }
}

fn from32(b: &[u8]) -> u32 {
    ((b[0] as u32) << 24) | ((b[1] as u32) << 16) | ((b[2] as u32) << 8) | (b[3] as u32)
}

enum Either<L, R> {
    Left(L),
    Right(R),
}
