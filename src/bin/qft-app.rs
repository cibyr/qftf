use std::env;
use anyhow::{Context, Result};
use iroh::Endpoint;
use iroh::NodeAddr;
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

use qft::*;


fn usage() -> ! {
    eprintln!("usage: qft-app TXCODE RXCODE");
    std::process::exit(2)
}

// App:
//  * "Scan" both QR codes
//  * (maybe) display metadata, have go button?
//  * Connect to receiver, send FT struct

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(EnvFilter::from_default_env())
        .init();

    let args: Vec<String> = env::args().collect();
    if args.len() != 3 {
        usage();
    }

    let txcode = &args[1];
    let tx_json = txcode.strip_prefix(TX_PREFIX).context("TXCODE must start with qft-tx:")?;

    let rxcode = &args[2];
    let rx_json = rxcode.strip_prefix(RX_PREFIX).context("RXCODE must start with qft-rx:?")?;
    let rx_addr: NodeAddr = serde_json::from_str(rx_json).context("couldn't parse rx json")?;

    let endpoint = Endpoint::builder()
        .bind()
        .await?;

    //  * connect to receiver's endpoint, send FT json
    tracing::info!("Connecting to receiver");
    let connection = endpoint.connect(rx_addr, ALPN).await?;
    tracing::info!("Connected!");
    let mut s = connection.open_uni().await?;
    s.write_all(tx_json.as_bytes()).await?;
    s.finish()?;
    s.stopped().await?;

    println!("Yay?");

    Ok(())
}
