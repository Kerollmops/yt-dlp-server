use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::bail;
use axum::extract::{Query, State};
use axum::routing::get;
use axum::Router;
use clap::Parser;
use serde::Deserialize;
use tokio::process::Command;
use tokio::task;
use tracing::{debug, error, info, trace};

/// The server that listens and downloads videos by using yt-dlp.
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// The adresse to listen to queries.
    #[arg(short, long, default_value = "0.0.0.0:8989")]
    listen: SocketAddr,

    /// The folder where videos should be downloaded.
    #[arg(short, long, default_value = "downloads")]
    download_folder: PathBuf,
}

#[derive(Deserialize)]
struct DownloadURL {
    url: String,
}

#[derive(Clone)]
struct AppState {
    download_folder: PathBuf,
}

async fn download_url(
    State(AppState { download_folder }): State<AppState>,
    Query(DownloadURL { url }): Query<DownloadURL>,
) {
    trace!("Started downloading {url:?}...");

    task::spawn(async move {
        let result = Command::new("yt-dlp")
            .args(["--paths", &download_folder.display().to_string()])
            .arg(&url)
            .status()
            .await;

        match result {
            Ok(s) if !s.success() => error!("There is an issue downloading {url:?}, status: {s}"),
            Err(err) => error!("There is an issue downloading {url:?}: {err:?}"),
            _ => info!("Finished downloading {url:?}"),
        }
    });
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let Args { listen, download_folder } = Args::parse();
    let app_state = AppState { download_folder };

    match Command::new("yt-dlp").arg("--version").output().await {
        Ok(output) => {
            let version = String::from_utf8(output.stdout).unwrap();
            let version = version.lines().next().unwrap();
            debug!("Running the server with `yt-dlp` version {version}");
        }
        Err(e) => bail!("While running `yt-dlp --version`: {e}"),
    };

    let app = Router::new().route("/", get(download_url)).with_state(app_state);
    debug!("Listening on {listen}...");
    axum::Server::bind(&listen).serve(app.into_make_service()).await?;

    Ok(())
}
