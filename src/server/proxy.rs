use std::io;

use tokio::net::{TcpListener, TcpSocket};

/* TODO:
 * 
*/

// TODO: Read page template

// TODO: Add config struct param
pub async fn startProxy() -> io::Result<()> {
    // TODO: take addr from passed config
    let listener = TcpListener::bind("127.0.0.1:8000").await?;
    loop {
        let (conn, _) = listner.accept().await?;
        handle_proxy(conn);
    }
}

async fn handle_proxy(conn: TcpSocket) {
    let server_conn = match TcpSocket::new_v4();
    {
        /* TODO: Set read deadline
    }
}
