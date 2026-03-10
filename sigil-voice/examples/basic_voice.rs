use tokio::time::sleep;
use std::time::Duration;
use sigil_voice::driver::CoreDriver;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Test voice connection with dummy parameters
    let driver = CoreDriver::connect(
        "wss://example.com/voice",
        "1234567890",
        "1234567890",
        "session_id",
        "token"
    ).await?;

    // Start mixing (this will test the encryption fixes)
    driver.start_mixing().await?;

    // Keep the connection alive for a short time to test
    sleep(Duration::from_secs(10)).await;

    Ok(())
}
