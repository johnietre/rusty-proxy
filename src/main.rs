mod proxy;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let addr = "127.0.0.1:8000";
    env_logger::init();
    let proxy = proxy::Proxy::new(addr)?;
    proxy.run().await?;
    Ok(())
}
