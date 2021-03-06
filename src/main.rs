use env_logger;

mod proxy;
use proxy::Proxy;

//async fn main() -> Result<(), Box<dyn std::error::Error>> {
#[tokio::main]
async fn main() -> std::io::Result<()> {
    env_logger::init();
    let px = Proxy::new("127.0.0.1:8080")?;
    px.run().await
}
