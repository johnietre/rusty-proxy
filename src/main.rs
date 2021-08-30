use std::net::{TcpListener, TcpStream};
use std::io::prelude::*;

fn main() {
    let listener = TcpListener::bind("127.0.0.1:8999").unwrap();
    for stream in listener.incoming() {
        let stream = stream.unwrap();
        handle(stream);
    }
}

fn handle(mut stream: TcpStream) {
    let mut buffer = [0; 5000];
    stream.read(&mut buffer).unwrap();
    println!("{}", String::from_utf8_lossy(&buffer[..]));
}
