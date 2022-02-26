use env_logger;

mod proxy;
use proxy::Proxy;

//async fn main() -> Result<(), Box<dyn std::error::Error>> {
#[tokio::main]
async fn main() -> std::io::Result<()> {
    env_logger::init();
    let proxy = Proxy::new("127.0.0.1:8000")?;
    proxy.run().await
}
