use tokio;
use tokio::io::{copy_bidirectional, AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

mod proxy;
use proxy::Proxy;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut proxy = Proxy::new("127.0.0.1:8000")?;
    let ln = TcpListener::bind("127.0.0.1:8000").await?;
    let (mut s1, _) = ln.accept().await?;
    let (mut s2, _) = ln.accept().await?;
    println!(
        "{:?}",
        copy_bidirectional(&mut s1, &mut s2)
            .await
            .unwrap_or_default()
    );
    Ok(())
}
