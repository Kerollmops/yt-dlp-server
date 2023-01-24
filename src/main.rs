use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Stdio;

use anyhow::{bail, Context};
use axum::extract::{Query, State};
use axum::routing::get;
use axum::Router;
use clap::Parser;
use once_cell::sync::Lazy;
use regex::{Captures, Regex};
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::task;
use tracing::{debug, error, info, trace};

/// Do a simple `--progress-template '%(progress)j'` to see the JSON and the possibilities.
const PROGRESS_TEMPLATE: &str = "%(progress.fragment_index)s/%(progress.fragment_count)s %(progress.eta)s %(progress.filename)s";
/// The regex that parses the above progress template.
static PROGRESS_REGEX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?P<index>\d+)/(?P<count>\d+) (?P<eta>\d+) (?P<filename>.+)").unwrap()
});

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
        let mut cmd = Command::new("yt-dlp")
            .args(["--paths", &download_folder.display().to_string()])
            .args(["-q", "--progress", "--newline", "--progress-template", PROGRESS_TEMPLATE])
            .args(["--", &url])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();

        let stdout = cmd.stdout.as_mut().unwrap();
        let stdout_reader = BufReader::new(stdout);
        let mut stdout_lines = stdout_reader.lines();
        while let Some(line) = stdout_lines.next_line().await.unwrap() {
            if let Some(captures) = PROGRESS_REGEX.captures(&line) {
                if let Ok(progress) = DownloadProgress::try_from(captures) {
                    println!("{:?}", progress);
                }
            }
        }

        match cmd.wait().await {
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

#[derive(Debug)]
struct DownloadProgress<'a> {
    index: usize,
    count: usize,
    eta: usize,
    filename: &'a str,
}

impl<'a> TryFrom<Captures<'a>> for DownloadProgress<'a> {
    type Error = anyhow::Error;

    fn try_from(cap: Captures<'a>) -> Result<DownloadProgress<'a>, Self::Error> {
        Ok(DownloadProgress {
            index: cap.name("index").context("missing `index`")?.as_str().parse()?,
            count: cap.name("count").context("missing `count`")?.as_str().parse()?,
            eta: cap.name("eta").context("missing `eta`")?.as_str().parse()?,
            filename: cap.name("filename").context("missing `filename`")?.as_str(),
        })
    }
}
