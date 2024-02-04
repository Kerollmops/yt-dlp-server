use std::collections::HashSet;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::{fs, include_str as include, thread, vec};

use anyhow::{bail, Context};
use askama::Template;
use axum::body::{Bytes, Full};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::{header, HeaderValue};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use axum::Router;
use chrono::{DateTime, Utc};
use clap::Parser;
use crossbeam_channel::bounded;
use heed::types::{SerdeJson, Str};
use heed::{Database, EnvOpenOptions};
use humantime::FormattedDuration;
use once_cell::sync::Lazy;
use regex::{Captures, Regex, RegexBuilder};
use reqwest::{redirect, ClientBuilder};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::broadcast;
use tokio::task;
use tracing::{debug, error, info, trace};
use url::Url;

/// Do a simple `--progress-template '%(progress)j'` to see the JSON and the possibilities.
const PROGRESS_TEMPLATE: &str = "%(progress.downloaded_bytes)s/%(progress.total_bytes)s %(progress.fragment_index)s/%(progress.fragment_count)s %(progress.eta)s %(progress.filename)s";
/// The regex that parses the above progress template.
static PROGRESS_REGEX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?P<downbytes>\w+)/(?P<totalbytes>\w+) (?P<fragindex>\w+)/(?P<fragcount>\w+) (?P<eta>\w+) (?P<filename>.+)").unwrap()
});
/// The set of URLs currently being downloaded, removed once the media has been downloaded.
static DOWNLOADING_URLS: Lazy<Mutex<HashSet<Url>>> = Lazy::new(Mutex::default);

type ChannelId = String;

/// The server that listens and downloads videos by using yt-dlp.
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// The adresse to listen to queries.
    #[arg(short, long, default_value = "0.0.0.0:8989")]
    listen: SocketAddr,

    /// The folder where the database is located.
    #[arg(long, default_value = ".")]
    database_folder: PathBuf,

    /// The folder where the videos of the subscriptions should be downloaded.
    #[arg(long, default_value = "downloads")]
    subscriptions_download_folder: PathBuf,

    /// The folder where direct download media should be downloaded.
    #[arg(long, default_value = "downloads")]
    direct_download_folder: PathBuf,
}

#[derive(Deserialize)]
struct DownloadURL {
    url: String,
}

#[derive(Deserialize)]
struct SubscribeURL {
    url: String,
    #[serde(with = "serde_regex")]
    restrict: Regex,
}

struct AppState {
    env: heed::Env,
    subscriptions: Database<Str, SerdeJson<ChannelSubscription>>,
    download_media: crossbeam_channel::Sender<(Url, PathBuf)>,
    direct_download_folder: PathBuf,
    progress: broadcast::Sender<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let Args { listen, database_folder, subscriptions_download_folder, direct_download_folder } =
        Args::parse();
    let (download_media_sender, download_media_receiver) = bounded(10);

    let database_path = database_folder.join(".yt-dlp-server.db");
    let _ = fs::create_dir_all(&database_path);
    let env = EnvOpenOptions::new().open(&database_path)?;
    let mut wtxn = env.write_txn()?;
    let subscriptions: Database<Str, SerdeJson<ChannelSubscription>> =
        env.create_database(&mut wtxn, None)?;
    wtxn.commit()?;

    let app_state = Arc::new(AppState {
        env: env.clone(),
        subscriptions,
        download_media: download_media_sender.clone(),
        direct_download_folder,
        progress: broadcast::channel(100).0,
    });

    match Command::new("yt-dlp").arg("--version").output().await {
        Ok(output) => {
            let version = String::from_utf8(output.stdout).unwrap();
            let version = version.lines().next().unwrap();
            debug!("Running the server with `yt-dlp` version {version}");
        }
        Err(e) => bail!("While running `yt-dlp --version`: {e:?}"),
    };

    thread::spawn(move || {
        let env = env.clone();
        let download_media = download_media_sender.clone();
        let download_folder = subscriptions_download_folder.clone();
        loop {
            if let Err(e) = fetch_new_medium(&env, subscriptions, &download_media, &download_folder)
            {
                error!("Failed fetching new medium: {e:?}");
            }

            thread::sleep(Duration::from_secs(30 * 60)); // 30 minutes
        }
    });

    // Listen and download the URLs sent through the channel
    let progress = app_state.progress.clone();
    task::spawn_blocking(move || {
        let progress = progress.clone();
        for (url, download_folder) in download_media_receiver {
            let progress = progress.clone();
            task::spawn(
                async move { download_url_with_ytdlp(url, progress, download_folder).await },
            );
        }
    });

    let app = Router::new()
        .route("/", get(index))
        .route("/download", get(download_url))
        .route("/subscribe", get(subscribe_url))
        .route("/unsubscribe", get(unsubscribe_id))
        .route("/subscriptions", get(list_subscriptions))
        .route("/moment.min.js", get(|| async { Js(include!("../js/moment.min.js")) }))
        .route("/bootstrap.min.js", get(|| async { Js(include!("../js/bootstrap.min.js")) }))
        .route("/bootstrap.min.css", get(|| async { Css(include!("../css/bootstrap.min.css")) }))
        .route("/websocket", get(websocket_handler))
        .with_state(app_state);
    info!("Listening on {listen}...");
    axum::Server::bind(&listen).serve(app.into_make_service()).await?;

    Ok(())
}

#[derive(Template)]
#[template(path = "downloads.html")]
struct IndexTemplate;

async fn index() -> IndexTemplate {
    IndexTemplate
}

async fn download_url(
    State(state): State<Arc<AppState>>,
    Query(DownloadURL { url }): Query<DownloadURL>,
) -> Redirect {
    let url = Url::parse(&url).unwrap();
    state.download_media.send((url, state.direct_download_folder.to_owned())).unwrap();
    Redirect::temporary("/")
}

async fn subscribe_url(
    State(state): State<Arc<AppState>>,
    Query(SubscribeURL { url, restrict }): Query<SubscribeURL>,
) -> Redirect {
    info!("Started subscribing to {url}...");
    let url = Url::parse(&url).unwrap();

    let client = ClientBuilder::new()
        .cookie_store(true)
        .redirect(redirect::Policy::limited(10))
        .build()
        .unwrap();

    let html = client.get(url.clone()).send().await.unwrap().text().await.unwrap();

    let document = scraper::Html::parse_document(&html);
    let selector = scraper::Selector::parse("link[rel=canonical]").unwrap();
    let channel_id = match document.select(&selector).next().and_then(|er| er.value().attr("href"))
    {
        Some(href) => {
            let url = Url::parse(href).unwrap();
            url.path_segments().and_then(|mut s| s.nth(1)).map(ToOwned::to_owned)
        }
        None => None,
    };

    if let Some(channel_id) = channel_id {
        info!("Extracted the channel id {channel_id}...");
        let channel_name = fetch_feed_title(&channel_id).unwrap().unwrap_or(channel_id.clone());
        let mut wtxn = state.env.write_txn().unwrap();
        let key = format!("{channel_id}{restrict}");
        if state.subscriptions.get(&wtxn, &key).unwrap().is_none() {
            let sub = ChannelSubscription {
                channel_name,
                channel_id,
                restrict,
                last_pull: chrono::Utc::now(),
            };
            state.subscriptions.put(&mut wtxn, &key, &sub).unwrap();
            wtxn.commit().unwrap();
        }
    }

    Redirect::temporary("/subscriptions")
}

#[derive(Deserialize)]
struct UnsubscribeId {
    id: String,
}

async fn unsubscribe_id(
    State(state): State<Arc<AppState>>,
    Query(UnsubscribeId { id }): Query<UnsubscribeId>,
) -> Redirect {
    let mut wtxn = state.env.write_txn().unwrap();
    state.subscriptions.delete(&mut wtxn, &id).unwrap();
    wtxn.commit().unwrap();
    Redirect::temporary("/subscriptions")
}

#[derive(Template)]
#[template(path = "subscriptions.html")]
struct SubscriptionsTemplate {
    subscriptions: Vec<Subscription>,
}

struct Subscription {
    id: String,
    channel_name: String,
    restrict: String,
    last_pull: FormattedDuration,
}

async fn list_subscriptions(State(state): State<Arc<AppState>>) -> SubscriptionsTemplate {
    let mut subscriptions = vec![];
    let rtxn = state.env.read_txn().unwrap();
    for result in state.subscriptions.iter(&rtxn).unwrap() {
        let (id, ChannelSubscription { channel_name, restrict, last_pull, .. }) = result.unwrap();
        let duration = {
            let duration = Utc::now() - last_pull;
            let duration = Duration::from_secs(duration.num_seconds().try_into().unwrap_or(0));
            humantime::Duration::from(duration)
        };

        subscriptions.push(Subscription {
            id: id.to_string(),
            channel_name,
            restrict: restrict.to_string(),
            last_pull: humantime::format_duration(*duration),
        });
    }
    SubscriptionsTemplate { subscriptions }
}

/// Removes the folder hierarchy and the extension. Keeps the filename only.
fn extract_clean_filename(path: impl AsRef<Path>) -> anyhow::Result<String> {
    Ok(path
        .as_ref()
        .with_extension("")
        .file_name()
        .context("can't extract the filename")?
        .to_string_lossy()
        .to_string())
}

async fn websocket_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    ws.on_upgrade(|socket| websocket(socket, state))
}

// This function deals with a single websocket connection, i.e., a single
// connected client / user, for which we will spawn two independent tasks (for
// receiving / sending chat messages).
async fn websocket(mut stream: WebSocket, state: Arc<AppState>) {
    let mut rx = state.progress.subscribe();
    while let Ok(msg) = rx.recv().await {
        // In any websocket error, break loop.
        if stream.send(Message::Text(msg)).await.is_err() {
            break;
        }
    }
}

#[derive(Debug)]
struct DownloadProgress<'a> {
    current: usize,
    total: usize,
    eta: usize,
    filename: &'a str,
}

impl Default for DownloadProgress<'static> {
    fn default() -> DownloadProgress<'static> {
        DownloadProgress { current: 0, total: 100, eta: 0, filename: "" }
    }
}

impl<'a> TryFrom<Captures<'a>> for DownloadProgress<'a> {
    type Error = anyhow::Error;

    fn try_from(cap: Captures<'a>) -> Result<DownloadProgress<'a>, Self::Error> {
        let best: anyhow::Result<_> = (|| {
            let downbytes =
                cap.name("downbytes").context("missing `downbytes`")?.as_str().parse()?;
            let totalbytes =
                cap.name("totalbytes").context("missing `totalbytes`")?.as_str().parse()?;
            Ok((downbytes, totalbytes))
        })();

        let worse: anyhow::Result<_> = (|| {
            let fragindex =
                cap.name("fragindex").context("missing `fragindex`")?.as_str().parse()?;
            let fragcount =
                cap.name("fragcount").context("missing `fragcount`")?.as_str().parse()?;
            Ok((fragindex, fragcount))
        })();

        let (current, total) = best.or(worse)?;

        let eta = match cap.name("eta").map(|eta| eta.as_str().parse()) {
            Some(Ok(eta)) => eta,
            Some(Err(e)) => {
                error!("invalid eta value `{}`", e);
                0
            }
            None => {
                error!("missing eta field");
                0
            }
        };

        Ok(DownloadProgress {
            current,
            total,
            eta,
            filename: cap.name("filename").context("missing `filename`")?.as_str(),
        })
    }
}

async fn download_url_with_ytdlp(
    url: Url,
    progress: broadcast::Sender<String>,
    download_folder: PathBuf,
) -> anyhow::Result<()> {
    info!("Started downloading {url}...");
    if !DOWNLOADING_URLS.lock().unwrap().insert(url.clone()) {
        return Ok(());
    }

    let mut cmd = Command::new("yt-dlp")
        .args(["--paths", &format!("temp:{}", std::env::temp_dir().display())])
        .args(["--paths", &download_folder.display().to_string()])
        .args(["--format", "bestvideo*+bestaudio/best"])
        .args(["-q", "--progress", "--newline", "--progress-template", PROGRESS_TEMPLATE])
        .args(["--", url.as_str()])
        .stdout(Stdio::piped())
        .spawn()?;

    let stdout = cmd.stdout.as_mut().unwrap();
    let stdout_reader = BufReader::new(stdout);
    let mut stdout_lines = stdout_reader.lines();
    while let Some(line) = stdout_lines.next_line().await? {
        trace!("progress line {line:?}");
        if let Some(captures) = PROGRESS_REGEX.captures(&line) {
            trace!("progress captures {captures:?}");
            let prg = match DownloadProgress::try_from(captures) {
                Ok(prg) => prg,
                Err(e) => {
                    error!("can't create download progress: {e}");
                    DownloadProgress::default()
                }
            };
            let filename = match extract_clean_filename(prg.filename) {
                Ok(filename) => filename,
                Err(e) => {
                    error!("can't extract clean filename: {e}");
                    String::from("N/A")
                }
            };
            let content = json!({
                "filename": filename,
                "url": url.as_str(),
                "percentage": (prg.current as f32) / (prg.total as f32) * 100.0,
                "eta": prg.eta,
            });
            let _ = progress.send(content.to_string());
        }
    }

    DOWNLOADING_URLS.lock().unwrap().remove(&url);

    match cmd.wait().await {
        Ok(s) if !s.success() => {
            error!("There is an issue downloading {url:?}, status: {s}")
        }
        Err(err) => error!("There is an issue downloading {url:?}: {err:?}"),
        _ => info!("Finished downloading {url}"),
    }

    Ok(())
}

/// Fetches the channel name, the URLs and publish datetime of the given YouTube channel.
fn fetch_filtered_feed(
    channel_id: ChannelId,
    restrict: Regex,
) -> anyhow::Result<Vec<(Url, DateTime<Utc>)>> {
    let client = reqwest::blocking::ClientBuilder::new()
        .cookie_store(true)
        .redirect(redirect::Policy::limited(10))
        .build()
        .context("building reqwest client")?;

    let url = format!("https://www.youtube.com/feeds/videos.xml?channel_id={channel_id}");
    let xml = client
        .get(url)
        .send()
        .context("sending get reqwest")?
        .bytes()
        .context("downloading request bytes")?;

    // Make the regex case-insensitive
    let restrict = RegexBuilder::new(restrict.as_str())
        .case_insensitive(true)
        .build()
        .context("building regex")?;

    let feed = feed_rs::parser::parse(&xml[..]).context("creating feed parser")?;
    let mut urls_published = Vec::new();
    for entry in feed.entries {
        if let Some(title) = entry.title.map(|t| t.content) {
            if restrict.is_match(&title) {
                if let Some((link, published)) = entry.links.first().zip(entry.published) {
                    if let Ok(url) = Url::parse(&link.href) {
                        urls_published.push((url, published));
                    }
                }
            }
        }
    }

    Ok(urls_published)
}

fn fetch_feed_title(channel_id: &ChannelId) -> anyhow::Result<Option<String>> {
    let xml =
        ureq::get(&format!("https://www.youtube.com/feeds/videos.xml?channel_id={channel_id}"))
            .call()?
            .into_string()?;
    let feed = feed_rs::parser::parse(xml.as_bytes())?;
    Ok(feed.title.map(|t| t.content))
}

fn fetch_new_medium(
    env: &heed::Env,
    subscriptions: Database<Str, SerdeJson<ChannelSubscription>>,
    download_media: &crossbeam_channel::Sender<(Url, PathBuf)>,
    download_folder: &Path,
) -> anyhow::Result<()> {
    // Pull the list of feeds to fetch and fetch the videos
    // that were published betwnee now and the last pull we did.
    let rtxn = env.read_txn().context("opening read txn")?;
    let now = chrono::Utc::now();
    for result in subscriptions.iter(&rtxn).context("creating subscription iterator")? {
        let (_key, sub) = result.context("decoding subscription entry")?;
        let ChannelSubscription { channel_id, last_pull, restrict, .. } = sub;
        let entries =
            fetch_filtered_feed(channel_id, restrict).context("fetching filtered feed")?;
        for (url, published) in entries {
            if published > last_pull {
                if let Err(e) = download_media.send((url, download_folder.to_owned())) {
                    error!("while sending in the channel: {e:?}");
                }
            }
        }
    }

    // Mark all of our subscriptions as recently-pulled
    let mut wtxn = env.write_txn().context("opening write txn")?;
    let mut iter =
        subscriptions.iter_mut(&mut wtxn).context("creating subscrition write iterator")?;
    while let Some(result) = iter.next() {
        let (key, mut sub) = result.context("decoding mutable subsbcription entry")?;
        sub.last_pull = now;
        unsafe {
            let key = key.to_string();
            iter.put_current(&key, &sub).context("in-place writing subscription entry")?
        };
    }
    drop(iter);
    wtxn.commit().context("committing subscription modifications")?;

    Ok(())
}

#[derive(Debug, Serialize, Deserialize)]
struct ChannelSubscription {
    channel_name: String,
    channel_id: String,
    #[serde(with = "serde_regex")]
    restrict: Regex,
    last_pull: DateTime<Utc>,
}

#[derive(Clone, Copy, Debug)]
pub struct Js<T>(pub T);

impl<T> IntoResponse for Js<T>
where
    T: Into<Full<Bytes>>,
{
    fn into_response(self) -> Response {
        (
            [(
                header::CONTENT_TYPE,
                HeaderValue::from_static(mime::APPLICATION_JAVASCRIPT_UTF_8.as_ref()),
            )],
            self.0.into(),
        )
            .into_response()
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Css<T>(pub T);

impl<T> IntoResponse for Css<T>
where
    T: Into<Full<Bytes>>,
{
    fn into_response(self) -> Response {
        (
            [(header::CONTENT_TYPE, HeaderValue::from_static(mime::TEXT_CSS_UTF_8.as_ref()))],
            self.0.into(),
        )
            .into_response()
    }
}
