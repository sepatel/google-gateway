#[tokio::main]
async fn main() -> anyhow::Result<()> {
    google_gateway::app::run().await
}
