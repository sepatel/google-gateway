#[tokio::main]
async fn main() -> anyhow::Result<()> {
    gmail_gateway::app::run().await
}
