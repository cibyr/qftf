use anyhow::Result;
use fltk::enums::Color;
use fltk::frame::Frame;
use fltk::image::SvgImage;
use fltk::misc::Progress as ProgressBar;
use fltk::{app, prelude::*, window::Window};
use human_repr::{HumanCount, HumanDuration, HumanThroughput};
use iroh::Endpoint;
use iroh::NodeAddr;
use iroh::Watcher;
use qrcode::render::svg;
use qrcode::QrCode;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use std::env;
use std::future::Future;
use std::io;
use std::path::Path;
use std::time::{Instant, Duration};
use tokio::fs::File;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

use qftf::*;

const URL_PREFIX_ENV: &str = "QFTF_URL_PREFIX";
const DEFAULT_URL_PREFIX: &str = "https://cibyr.github.io/qftf-web/";
const UPDATE_PERIOD: Duration = Duration::from_millis(500);

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
    finished: bool,
}

/// Copy data from a reader to a writer with progress callbacks
///
/// Similar to `tokio::io::copy`, but calls the provided callback function
/// whenever a chunk of data is copied, providing the total bytes copied so far.
///
/// # Parameters
/// * `reader` - The source implementing AsyncRead
/// * `writer` - The destination implementing AsyncWrite
/// * `on_progress` - A callback function that takes the total bytes copied so far
///
/// # Returns
/// The total number of bytes copied
pub async fn copy_with_progress<R, W, F, Fut>(
    mut reader: R,
    mut writer: W,
    mut on_progress: F,
) -> io::Result<u64>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
    F: FnMut(u64) -> Fut,
    Fut: Future<Output = ()>,
{
    let buf_size = 8 * 1024; // Default 8KB buffer
                             // TODO: MaybeUninit buffer
    let mut buffer = vec![0u8; buf_size];
    let mut total_bytes = 0u64;

    loop {
        let bytes_read = reader.read(&mut buffer).await?;
        if bytes_read == 0 {
            break;
        }

        writer.write_all(&buffer[..bytes_read]).await?;

        total_bytes += bytes_read as u64;

        // Call the progress callback with the current total
        on_progress(total_bytes).await;
    }

    writer.flush().await?;
    Ok(total_bytes)
}

// Draw the UI
fn show_window(url: &str, title: &str) -> (Window, Frame, ProgressBar) {
    let code = QrCode::new(url).unwrap();
    let svg = code
        .render::<svg::Color>()
        .quiet_zone(true)
        .min_dimensions(400, 400)
        .build();
    let image = SvgImage::from_data(&svg).unwrap();
    let width = image.width();
    let height = image.height();

    let mut wind = Window::default().with_size(width, height).with_label(title);

    let mut frame = Frame::default().with_size(width, height).center_of(&wind);
    frame.set_image(Some(image));

    let mut pb = ProgressBar::default().with_size(width, 20);
    pb.set_selection_color(Color::Blue);
    pb.hide();

    wind.end();
    wind.show();

    (wind, frame, pb)
}

// The plan:
// Parse arguments (filename to send, no args to receive)
// Sender:
//  * create endpoint, stat file -> fill out FileTransfer struct
//  * display QR code ("qftf-tx:<encoded struct>")
//  * listen on the endpoint, waiting for receiver to supply token
//  * stream the file over the connection
//    * (maybe) display progress
//  * (maybe) append some kind of hash or CRC?
// Receiver:
//  * create endpoint
//  * display QR code ("qftf-rx:<NodeAddr>")
//  * listen on the endpoint, waiting for app to supply FT struct
//  * connect to sender's endpoint, send token
//  * receive the file over the connection (stream to disk)
//    * display progress
//  * (maybe) check hash/CRC?
// Web App:
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

    // Wait for us to have a home relay
    let _relay_url = endpoint.home_relay().initialized().await?;

    let transfer = FileTransfer {
        node: endpoint.node_addr().initialized().await?,
        name: file_name.to_string_lossy().into_owned(),
        size: file_size,
        token,
    };

    println!("Sending {:?}", &transfer);
    let transfer_json = serde_json::to_string(&transfer)?;
    let env_url = env::var(URL_PREFIX_ENV);
    let url_prefix = env_url.as_deref().unwrap_or(DEFAULT_URL_PREFIX);
    let url = format!("{url_prefix}#{TX_PREFIX}{transfer_json}");
    println!("URL: {url}");

    let app = app::App::default();
    let title = format!("QFTF - Sending {}", transfer.name);
    let (_window, mut frame, mut pb) = show_window(&url, &title);

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
            let (mut s, mut r) = match connection.accept_bi().await {
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
            let _bytes_sent = copy_with_progress(file, &mut s, |bytes_sent| {
                let name = transfer.name.clone();
                let size = transfer.size;
                async move {
                    progress_sender.send(Progress {
                        name,
                        bytes_sent,
                        total_size: size,
                        finished: false,
                    });
                }
            })
            .await?;
            s.finish()?;
            s.stopped().await?;
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

    let mut started = false;
    let mut start = Instant::now();
    let mut last_update = start - UPDATE_PERIOD;
    while app.wait() {
        if let Some(progress) = progress_receiver.recv() {
            if progress.finished {
                app::quit();
                break;
            }
            let now = Instant::now();
            if !started {
                pb.set_minimum(0.0);
                pb.set_maximum(progress.total_size as f64);
                pb.show();
                start = now;
                started = true;
                continue;
            }
            pb.set_value(progress.bytes_sent as f64);
            if now - last_update < UPDATE_PERIOD {
                continue;
            }

            let seconds_so_far = (now - start).as_secs_f64();
            let rate = progress.bytes_sent as f64 / seconds_so_far;
            let remaining_bytes = progress.total_size - progress.bytes_sent;
            let remaining_time = Duration::try_from_secs_f64(remaining_bytes as f64 / rate).map(|d| d.human_duration().to_string());
            frame.set_image::<SvgImage>(None);
            frame.set_label(&format!(
                "Sending {}\n
                {} / {}\n
                {} remaining ({})",
                progress.name, progress.bytes_sent.human_count_bytes(),
                progress.total_size.human_count_bytes(),
                remaining_time.as_deref().unwrap_or("forever"),
                rate.human_throughput_bytes()
            ));
            last_update = now;
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

    // Wait for us to have a home relay
    let _relay_url = endpoint.home_relay().initialized().await?;

    //  * display QR code ("qftf-rx:<NodeAddr>")
    let node_addr = endpoint.node_addr().initialized().await?;
    let env_url = env::var(URL_PREFIX_ENV);
    let url_prefix = env_url.as_deref().unwrap_or(DEFAULT_URL_PREFIX);
    let url = format!(
        "{}#{}{}",
        url_prefix,
        RX_PREFIX,
        serde_json::to_string(&node_addr)?
    );

    println!("URL: {url}");

    let app = app::App::default();
    let title = "QFTF - Waiting to receive...".to_string();
    let (mut window, mut frame, _pb) = show_window(&url, &title);

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
            let f = File::create_new(&transfer.name).await?;
            copy_with_progress(r, f, |bytes_sent| {
                let name = transfer.name.clone();
                let size = transfer.size;
                async move {
                    progress_sender.send(Progress {
                        name,
                        bytes_sent,
                        total_size: size,
                        finished: false,
                    });
                }
            })
            .await?;
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

    let mut got_name = false;
    while app.wait() {
        if let Some(progress) = progress_receiver.recv() {
            if progress.finished {
                app::quit();
                break;
            }
            if !got_name {
                window.set_label(&format!("QFTF - Receiving {}", progress.name));
                got_name = true;
            }

            frame.set_image::<SvgImage>(None);
            frame.set_label(&format!(
                "Receiving {}\n
                {} / {} bytes",
                progress.name, progress.bytes_sent, progress.total_size
            ));
        }
    }

    println!("Done!");
    Ok(())
}

fn usage() -> ! {
    eprintln!("usage: qftf [file]");
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
