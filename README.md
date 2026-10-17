# Tinysound

A lightweight Windows desktop music app for SoundCloud, Spotify, and YouTube Music, built with Rust and Microsoft WebView2.

## Features

- Three music services in one window
- Light and dark themes
- Windows audio output selection
- Stereo, left-only and right-only channel controls for the app
- Optional Discord activity for SoundCloud and YouTube Music

## Build and run

Requires Windows 10/11, Microsoft WebView2 Runtime, Rust, and the Visual Studio C++ build tools.

```powershell
cargo build --release --locked
Copy-Item extensions target/release/extensions -Recurse -Force
./target/release/tinysound.exe
```

Keep the `extensions` folder beside the executable. Sign in to music services within the app. Service subscriptions and playback restrictions still apply.

## Discord

Open the Discord desktop app, then click **Link Discord**. No bot token or personal application ID is required: the app uses the shared public application ID `1505487506445963324`. Sharing starts disabled at every launch. Spotify activity is excluded. Discord controls the application name displayed on your profile. Discord activity privacy settings may need to allow sharing.

## Audio controls

Start playback before selecting an output device. Restart playback if Windows does not move the existing stream. Left-only and right-only mute the other channels; they do not downmix both sides into one ear. Downstream routing through Voicemeeter or Discord can affect stereo playback.

## Development status

Experimental Windows application. Automated checks cover routing interface availability, channel restoration, and Discord IPC handling. Audible output routing and visible Discord presence require a live playback check.

## Credits and licensing

Published by 5lwf. Application code is MIT licensed; see LICENSE. The Windows audio policy interface reference and its licensing terms are preserved in EarTrumpet-NOTICE.txt. Bundled uBlock Origin Lite extension code retains its own license and notices in extensions/. Third-party music services and trademarks belong to their owners. This project is not affiliated with those services.
