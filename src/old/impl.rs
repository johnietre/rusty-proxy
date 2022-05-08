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
                let s = ServerInfo::new("mbp", "192.168.1.130:9000");
                m.store(s.name.clone(), s);
                let s = ServerInfo::new("rb15", "192.168.1.126:9000");
                m.store(s.name.clone(), s);
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
        #[cfg(target = "unix")]
        {
        socket.set_reuse_port(true)?;
        }
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
            //let mut buf_writer = BufWriter::with_capacity(req.len(), server);
            let mut buf_writer = BufWriter::with_capacity(BUFFER_SIZE, server);
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
                header_size += line.len(); // TODO: Could yield incorrect result when line len == 2
                if line.len() == 2 {
                    // TODO: Not handling for headers greater than buffer size (possibly don't need
                    // to handle for that)

                    // Add the Forwarded header
                    let header = format!("Forwarded: for={}\r\n\r\n", client_addr.ip());
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
                let fut = if !got_content_length {
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
                    Ok(_) => (),
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
                logm(Level::Trace, format!("Reading rest of total {} bytes", total_size));
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
            // Split the server stream and pipe the connections
            let (ref mut server_read, ref mut server_write) = tio::split(server);
            // Client always returns Either::Left and server Either::Right
            let client_fut = async {
                loop {
                    match client_read.read(&mut req_buf).await {
                        Ok(0) => break Either::Left(Ok(0)), // NOTE: Ignore?
                        Ok(l) => {
                            println!("{}", String::from_utf8_lossy(&req_buf[..l]));
                            match Self::parse_site_name(&req_buf[..l]).await {
                                Ok((name, _)) => {
                                    if name == "favicon.ico" {
                                        continue;
                                    }
                                    if name != site_name {
                                        println!("Old: {}\tNew: {}", site_name, name);
                                        break Either::Left(Ok(l));
                                    }
                                    println!("Same: {}", name);
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
                            //Ok(l) => bytes_read = l, // Unreachable right now
                            Ok(l) => {
                                logm(Level::Info, format!("client_fut: read {} bytes from server", l));
                                bytes_read = l; // Unreachable right now
                            }
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
                            //Ok(l) => bytes_read = l, // Unreachable right now
                            Ok(l) => {
                                logm(Level::Info, format!("server_fut: read {} bytes from client", l));
                                bytes_read = l; // Unreachable right now
                            }
                        }
                        Either::Right(res) => match res {
                            //Ok(l) => bytes_read = l,
                            Ok(l) => {
                                logm(Level::Info, format!("server_fut: read {} bytes from server", l));
                                bytes_read = l; // Unreachable right now
                            }
                            Err(e) => {
                                logm(Level::Info, format!("error writing to server: {}", e));
                                let _ = client_write.write(INTERNAL_SERVER_ERROR_RESPONSE).await;
                            }
                        }
                    }
                }
            }
            logm(Level::Trace, "done".into());
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
        let first_line = match String::from_utf8(req[..line_end_pos + 1].to_vec()) {
            Ok(line) => line,
            Err(_) => return Err(BAD_REQUEST_RESPONSE),
        };
        let mut parts = req[..line_end_pos - 2].split(|&b| b == b' ');
        if parts.next().is_none() {
            return Err(BAD_REQUEST_RESPONSE);
        }
        if let Some(part) = parts.next() {
            if part.len() == 0 {
                Err(BAD_REQUEST_RESPONSE)
            } else if part.len() == 1 {
                Ok((String::from(""), first_line))
            } else {
                /*
                if let Some(pos) = part[1..].iter().position(|&b| b == b'/' || b == b' ') {
                    match String::from_utf8(part[1..pos + 1].to_vec()) {
                        Ok(name) => {
                            if part[pos] == b'/' {
                                Ok((name.clone(), first_line.replacen(&(name + "/"), "", 1)))
                            } else {
                                Ok((name.clone(), first_line.replacen(&name, "", 1)))
                            }
                        }
                        //Err(_) => Err(BAD_REQUEST_RESPONSE),
                        Err(e) => {
                            log!(Level::Trace, "erroro parsing site: {}", e);
                            Err(BAD_REQUEST_RESPONSE)
                        }
                    }
                } else {
                    return Err(BAD_REQUEST_RESPONSE);
                }
                */
                let pos = part[1..].iter().position(|&b| b == b'/').unwrap_or(part.len() - 1);
                match String::from_utf8(part[1..pos + 1].to_vec()) {
                    Ok(name) => {
                        if part[pos] == b'/' {
                            Ok((name.clone(), first_line.replacen(&(name + "/"), "", 1)))
                        } else {
                            Ok((name.clone(), first_line.replacen(&name, "", 1)))
                        }
                    }
                    Err(_) => Err(BAD_REQUEST_RESPONSE),
                }
            }
        } else {
            Err(BAD_REQUEST_RESPONSE)
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
