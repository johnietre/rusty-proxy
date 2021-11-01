mod proxy;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let addr = "127.0.0.1:8000";
    env_logger::init();
    let proxy = proxy::Proxy::new(addr)?;
    proxy.add_path(
        proxy::ServerConn::new("/path".into(), "127.0.0.1:8080", "http".into()).unwrap(),
    )?;
    proxy.run().await?;
    Ok(())
}
