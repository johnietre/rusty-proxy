use log::{debug};
use std::collections::HashMap;
use std::error::Error;
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::{Arc, RwLock, atomic::{AtomicBool, Ordering}};
use tokio::net::{TcpListener, TcpStream};

const BUFFER_SIZE: usize = 2048;

pub struct Proxy {
    addr: SocketAddr,
    routes: Arc<RwLock<HashMap<String, Route>>>,
    running: AtomicBool,
}

impl Proxy {
    pub fn new(addr: impl ToSocketAddr) -> Result<Self, Box<dyn Error>> {
        // Convert the given address
        let addr_iter;
        match addr.to_socket_addr() {
            Ok(iter) => addr_iter = iter,
            Err(e) => return e,
        }
        // Get the first address from the iterator
        let addr;
        match addr_iter.next() {
            Some(a) => addr = a,
            _ => panic!("Error parsing addr, no address in iterator"),
        }
        // Create the proxy
        Proxy {
            addr,
            routes: Arc::new(RwLock::new(HashMap::new())),
            running: AtomicBool::new(false),
        }
    }

    pub async fn run(&self) -> Result<(), Box<dyn Error>> {
        match self.running.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed) {
            Ok(_) => (),
            Err(_) => return Box::new(ProxyError {
                what: "Proxy already running"
            }),
        }
        let listener = TcpListener::bind(self.addr).await?;
        loop {
            let (stream, addr) = listener.accept().await?;
            self.handle(stream, addr);
        }
        Ok(())
    }

    async fn handle(&self, _stream: TcpStream, _addr: SocketAddr) {
        let mut _buffer = [0u8; BUFFER_SIZE];
        //
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
    handler: Handler,
}

impl Route {
    pub fn new(path: String, handler: Handler) -> Self {
        Self {
            path,
            handler,
        }
    }
}

type Handler = fn(TcpStream);

pub struct ProxyError {
    what: String,
}

impl Error for ProxyError {}
