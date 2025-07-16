use anyhow::Result;
use fltk::frame::Frame;
use fltk::image::SvgImage;
use fltk::{app, prelude::*, window::Window};
use iroh::Endpoint;
use iroh::NodeAddr;
use qrcode::render::svg;
use qrcode::QrCode;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use std::env;
use std::path::Path;
use std::io;
use tokio::fs::File;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_util::sync::CancellationToken;
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

use qft::*;

const URL_PREFIX: &str = "https://rambunctiousness.com/qft/";

#[derive(Debug, Serialize, Deserialize)]
struct FileTransfer {
    /// Public key of sender
    node: NodeAddr,
    /// Name of the file to transfer
    name: String,
    /// File size in bytes
    size: u64,
    /// Random 8 bytes
    token: [u8; 8],
}

struct Progress {
    name: String,
    bytes_sent: u64,
    total_size: u64,
    finished: bool
}

/// Copy from a reader to a quinn stream.
///
/// Will send a reset to the other side if the operation is cancelled, and fail
/// with an error.
///
/// Returns the number of bytes copied in case of success.
async fn copy_to_quinn(
    mut from: impl AsyncRead + Unpin,
    mut send: quinn::SendStream,
    token: CancellationToken,
) -> io::Result<u64> {
    tracing::trace!("copying to quinn");
    tokio::select! {
        res = tokio::io::copy(&mut from, &mut send) => {
            let size = res?;
            send.finish()?;
            Ok(size)
        }
        _ = token.cancelled() => {
            // send a reset to the other side immediately
            send.reset(0u8.into()).ok();
            Err(io::Error::new(io::ErrorKind::Other, "cancelled"))
        }
    }
}

/// Copy from a quinn stream to a writer.
///
/// Will send stop to the other side if the operation is cancelled, and fail
/// with an error.
///
/// Returns the number of bytes copied in case of success.
async fn copy_from_quinn(
    mut recv: quinn::RecvStream,
    mut to: impl AsyncWrite + Unpin,
    token: CancellationToken,
) -> io::Result<u64> {
    tokio::select! {
        res = tokio::io::copy(&mut recv, &mut to) => {
            Ok(res?)
        },
        _ = token.cancelled() => {
            recv.stop(0u8.into()).ok();
            Err(io::Error::new(io::ErrorKind::Other, "cancelled"))
        }
    }
}

// The plan:
// Parse arguments (filename to send, no args to receive)
// Sender:
//  * create endpoint, stat file -> fill out FileTransfer struct
//  * display QR code ("qft-tx:<encoded struct>")
//  * listen on the endpoint, waiting for receiver to supply token
//  * stream the file over the connection
//    * (maybe) display progress
//  * (maybe) append some kind of hash or CRC?
// Receiver:
//  * create endpoint
//  * display QR code ("qft-rx:<NodeAddr>")
//  * listen on the endpoint, waiting for app to supply FT struct
//  * connect to sender's endpoint, send token
//  * receive the file over the connection (stream to disk)
//    * (maybe) display progress
//  * (maybe) check hash/CRC?
// App:
//  * Scan both QR codes
//  * (maybe) display metadata, have go button?
//  * Connect to receiver, send FT struct

async fn send_file(path: &str) -> Result<()> {
    // Create an endpoint, it allows creating and accepting
    // connections in the iroh p2p world
    let endpoint = Endpoint::builder()
        .alpns(vec![ALPN.to_vec()])
        .bind()
        .await?;

    // Open the file, learn its length
    let path = Path::new(path);
    let file = File::open(path).await?;
    let file_name = path.file_name().expect("file has no name");
    let file_size = file.metadata().await?.len();

    // Generate a random 8-byte token
    let mut token = [0u8; 8];
    let mut rng = rand::rng();
    rng.fill_bytes(&mut token);

    let transfer = FileTransfer {
        node: endpoint.node_addr().await?,
        name: file_name.to_string_lossy().into_owned(),
        size: file_size,
        token,
    };

    println!("Sending {:?}", &transfer);
    let transfer_json = serde_json::to_string(&transfer)?;
    let url = format!("{}#{}{}", URL_PREFIX, TX_PREFIX, transfer_json);
    println!("URL: {}", url);

    // Draw the UI
    let code = QrCode::new(url).unwrap();
    let svg = code.render::<svg::Color>().min_dimensions(400, 400).build();

    let app = app::App::default();
    let mut wind = Window::new(100, 100, 400, 400, "QFT");

    let mut frame = Frame::default().with_size(400, 400).center_of(&wind);
    let image = SvgImage::from_data(&svg).unwrap();
    frame.set_image(Some(image));

    wind.end();
    wind.show();

    let (progress_sender, progress_receiver) = app::channel::<Progress>();

    tokio::spawn(async move {
        loop {
            let Some(connecting) = endpoint.accept().await else {
                break;
            };
            let connection = match connecting.await {
                Ok(connection) => connection,
                Err(cause) => {
                    tracing::warn!("error accepting connection: {}", cause);
                    // if accept fails, we want to continue accepting connections
                    continue;
                }
            };
            let remote_node_id = &connection.remote_node_id()?;
            tracing::info!("got connection from {}", remote_node_id);
            let (s, mut r) = match connection.accept_bi().await {
                Ok(x) => x,
                Err(cause) => {
                    tracing::warn!("error accepting stream: {}", cause);
                    // if accept_bi fails, we want to continue accepting connections
                    continue;
                }
            };
            tracing::info!("accepted stream from {}", remote_node_id);
            // read the token and verify it
            let mut buf = [0u8; 8];
            r.read_exact(&mut buf).await?;
            anyhow::ensure!(buf == transfer.token, "invalid token");

            progress_sender.send(Progress {
                name: transfer.name.clone(),
                bytes_sent: 0,
                total_size: transfer.size,
                finished: false,
            });

            // Send the file
            let token = CancellationToken::new();
            let _bytes_sent = copy_to_quinn(file, s, token).await?;
            // TODO: check bytes_sent, get rid of cancellation token?
            // TODO: progress
            tracing::info!("Transfer complete!");
            progress_sender.send(Progress {
                name: transfer.name,
                bytes_sent: transfer.size,
                total_size: transfer.size,
                finished: true,
            });

            break;
        }

        Ok(())
    });

    while app.wait() {
        if let Some(progress) = progress_receiver.recv() {
            if progress.finished {
                app::quit();
                break;
            }

            frame.set_image::<SvgImage>(None);
            frame.set_label(&format!("Sending {}...", progress.name));
        }
    }

    println!("Done!");
    Ok(())
}

async fn receive_file() -> Result<()> {
    //  * create endpoint
    let endpoint = Endpoint::builder()
        .alpns(vec![ALPN.to_vec()])
        .bind()
        .await?;

    //  * display QR code ("qft-rx:<NodeAddr>")
    let node_addr = endpoint.node_addr().await?;
    let url = format!("{}#{}{}", URL_PREFIX, RX_PREFIX, serde_json::to_string(&node_addr)?);

    println!("URL: {}", url);

    // Draw the UI
    let code = QrCode::new(url).unwrap();
    let svg = code.render::<svg::Color>().min_dimensions(400, 400).build();

    let app = app::App::default();
    let mut wind = Window::new(100, 100, 400, 400, "QFT");

    let mut frame = Frame::default().with_size(400, 400).center_of(&wind);
    let image = SvgImage::from_data(&svg).unwrap();
    frame.set_image(Some(image));

    wind.end();
    wind.show();

    let (progress_sender, progress_receiver) = app::channel::<Progress>();

    tokio::spawn(async move {
        loop {
            //  * listen on the endpoint, waiting for app to supply FT struct
            let Some(connecting) = endpoint.accept().await else {
                break;
            };
            let connection = match connecting.await {
                Ok(connection) => connection,
                Err(cause) => {
                    tracing::warn!("error accepting connection: {}", cause);
                    // if accept fails, we want to continue accepting connections
                    continue;
                }
            };
            let remote_node_id = &connection.remote_node_id()?;
            tracing::info!("got connection from {}", remote_node_id);
            let mut rs = match connection.accept_uni().await {
                Ok(x) => x,
                Err(cause) => {
                    tracing::warn!("error accepting stream: {}", cause);
                    // if accept_uni fails, we want to continue accepting connections
                    continue;
                }
            };
            tracing::info!("accepted stream from {}", remote_node_id);
            // read tx json
            let tx_json = rs.read_to_end(MAX_QR_BYTES).await?;
            let tx_json = String::from_utf8(tx_json)?;
            let transfer: FileTransfer = serde_json::from_str(&tx_json)?;

            //  * connect to sender's endpoint, send token
            let connection = endpoint.connect(transfer.node, ALPN).await?;
            tracing::info!("Connected!");
            let (mut s, r) = connection.open_bi().await?;
            s.write_all(&transfer.token).await?;

            //  * receive the file over the connection (stream to disk)
            let token = CancellationToken::new();
            let f = File::create_new(&transfer.name).await?;
            copy_from_quinn(r, f, token).await?;
            // TODO: progress
            tracing::info!("Transfer complete!");
            progress_sender.send(Progress {
                name: transfer.name,
                bytes_sent: transfer.size,
                total_size: transfer.size,
                finished: true,
            });
            break;
        }

        Ok::<(), anyhow::Error>(())
    });

    while app.wait() {
        if let Some(progress) = progress_receiver.recv() {
            if progress.finished {
                app::quit();
                break;
            }

            frame.set_image::<SvgImage>(None);
            frame.set_label(&format!("Receiving {}...", progress.name));
        }
    }

    println!("Done!");
    Ok(())
}

fn usage() -> ! {
    eprintln!("usage: qft [file]");
    std::process::exit(2)
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(EnvFilter::from_default_env())
        .init();

    let args: Vec<String> = env::args().collect();
    match args.len() {
        1 => receive_file().await,
        2 => send_file(&args[1]).await,
        _ => usage(),
    }
}
