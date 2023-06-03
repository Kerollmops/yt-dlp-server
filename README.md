# yt-dlp-server
A small server that can download any media by using yt-dlp


<p align="center">
<img align="left" width="45%" alt="The ongoing downloads" src="/screenshots/ongoing-downloads.PNG">
<img align="right" width="45%" alt="The list of subscriptions" src="/screenshots/subscriptions.PNG">
</p>

## Installation

Install the crate global to make it available to the current user.

```sh
cargo install --path .
```

## Running it on mac os

Customize the `$USER` variable of the _launched.yt-dlp-server.plist_ file then load it.

```sh
launchctl load -w launched.yt-dlp-server.plist
launchctl list | grep yt-dlp-server
launchctl start launched.yt-dlp-server
```