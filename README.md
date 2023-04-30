# yt-dlp-server
A small server that can download any media by using yt-dlp

## Installation

Install the crate global to make it available to the current user.

```sh
cargo install --path .
```

## Running it on mac os

Customize the `$USER` variable of the _launched.yt-dlp-server.plist_ file then load it.

```sh
launchctl load launched.yt-dlp-server.plist
launchctl list | grep yt-dlp-server
launchctl start launched.yt-dlp-server
```