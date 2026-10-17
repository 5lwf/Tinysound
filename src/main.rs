#![windows_subsystem = "windows"]
mod audio;
mod discord;
mod routing;
use base64::Engine;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::path::PathBuf;

fn shell_navigation_allowed(url: &str) -> bool {
    if url == "about:blank" {
        return true;
    }
    // WebView2 can expose NavigateToString as a data URL before committing about:blank.
    // Permit only our compiled HTML, not arbitrary data URLs or remote pages.
    let Some((header, payload)) = url.split_once(',') else {
        return false;
    };
    if !matches!(
        header,
        "data:text/html;base64"
            | "data:text/html;charset=utf-8;base64"
            | "data:text/html;charset=UTF-8;base64"
    ) {
        return false;
    }
    base64::engine::general_purpose::STANDARD
        .decode(payload)
        .is_ok_and(|bytes| bytes == include_bytes!("shell.html"))
}
use webview2_com::{
    Microsoft::Web::WebView2::Win32::{
        ICoreWebView2_8, ICoreWebView2_13, ICoreWebView2Profile7, ICoreWebView2Settings9,
    },
    ProfileAddBrowserExtensionCompletedHandler, take_pwstr,
};
use windows::core::{BOOL, HSTRING, Interface, PWSTR};
use winit::{
    application::ApplicationHandler,
    event::WindowEvent,
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy},
    window::{Icon, ResizeDirection, Window, WindowId},
};
use wry::{
    MemoryUsageLevel, NewWindowResponse, PageLoadEvent, Rect, WebContext, WebView, WebViewBuilder,
    WebViewBuilderExtWindows, WebViewExtWindows,
};

#[derive(Clone, Copy, PartialEq, Debug)]
enum Service {
    SoundCloud,
    Spotify,
    YouTubeMusic,
}
impl Service {
    fn url(self) -> &'static str {
        match self {
            Self::SoundCloud => "https://soundcloud.com/discover",
            Self::Spotify => "https://open.spotify.com/",
            Self::YouTubeMusic => "https://music.youtube.com/",
        }
    }
    fn name(self) -> &'static str {
        match self {
            Self::SoundCloud => "SoundCloud",
            Self::Spotify => "Spotify",
            Self::YouTubeMusic => "YouTube Music",
        }
    }
    fn key(self) -> &'static str {
        match self {
            Self::SoundCloud => "soundcloud",
            Self::Spotify => "spotify",
            Self::YouTubeMusic => "youtube",
        }
    }
}
enum Event {
    Routing(routing::Notice),
    Discord(discord::Notice),
    Metadata(Service, String),
    Audio(audio::Mode, String),
    Command(String),
    Extension(Result<String, String>),
    Loaded,
}
struct App {
    routing: routing::Router,
    output_devices: Vec<String>,
    output_index: Option<usize>,
    output_status: String,
    discord: discord::Presence,
    discord_enabled: bool,
    discord_status: String,
    audio: audio::Controller,
    audio_mode: audio::Mode,
    audio_status: String,
    player: Option<WebView>,
    shell: Option<WebView>,
    window: Option<Window>,
    context: Option<WebContext>,
    proxy: EventLoopProxy<Event>,
    service: Service,
    extension_id: Option<String>,
    settings: bool,
    extension_page: bool,
}
fn player_bounds(window: &Window) -> Rect {
    let size = window.inner_size().to_logical::<f64>(window.scale_factor());
    Rect {
        position: wry::dpi::LogicalPosition::new(15.0, 105.0).into(),
        size: wry::dpi::LogicalSize::new(
            (size.width - 30.0).max(1.0),
            (size.height - 120.0).max(1.0),
        )
        .into(),
    }
}
fn command_allowed(command: &str) -> bool {
    if let Some(index) = command.strip_prefix("output-device:") {
        return index.len() <= 3 && index.parse::<usize>().is_ok_and(|n| n < 128);
    }
    matches!(
        command,
        "ready"
            | "soundcloud"
            | "spotify"
            | "youtube"
            | "return"
            | "settings"
            | "blocker"
            | "back"
            | "reload"
            | "discord-toggle"
            | "output-refresh"
            | "audio-stereo"
            | "audio-left"
            | "audio-right"
            | "close"
            | "minimize"
            | "maximize"
            | "drag"
            | "resize-n"
            | "resize-s"
            | "resize-e"
            | "resize-w"
            | "resize-ne"
            | "resize-nw"
            | "resize-se"
            | "resize-sw"
    )
}
impl App {
    fn update_shell(&self) {
        if let Some(shell) = &self.shell {
            let status = if self.settings {
                "Your listening space".to_string()
            } else if self.extension_page {
                "uBlock Origin Lite · external extension settings".to_string()
            } else {
                format!("{} · one player, less overhead", self.service.name())
            };
            let mode = match self.audio_mode {
                audio::Mode::Stereo => "stereo",
                audio::Mode::Left => "left",
                audio::Mode::Right => "right",
            };
            let data = serde_json::json!({"service":self.service.key(),"settings":self.settings,"blocker":self.extension_id.is_some(),"status":status,"audioMode":mode,"audioStatus":self.audio_status,"discordEnabled":self.discord_enabled,"discordStatus":self.discord_status,"outputDevices":self.output_devices,"outputIndex":self.output_index,"outputStatus":self.output_status});
            let _ =
                shell.evaluate_script(&format!("window.updateShell&&window.updateShell({data})"));
        }
    }
    fn show_player(&mut self) {
        self.settings = false;
        if let Some(player) = &self.player {
            let _ = player.set_visible(true);
        }
        self.update_shell();
    }
    fn switch(&mut self, service: Service) {
        self.discord.update(None);
        let changed = self.service != service || self.extension_page;
        self.service = service;
        self.extension_page = false;
        self.show_player();
        if changed {
            if let Some(player) = &self.player {
                if let Err(e) = player.load_url(service.url()) {
                    show_message(&format!("Could not open {}: {e}", service.name()));
                }
            }
        }
    }
    fn resize(&self) {
        if let Some(window) = &self.window {
            round_window(window);
        }
        if let (Some(player), Some(window)) = (&self.player, &self.window) {
            let _ = player.set_bounds(player_bounds(window));
        }
    }
}
impl ApplicationHandler<Event> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let window = event_loop
            .create_window(
                Window::default_attributes()
                    .with_title("Tinysound")
                    .with_decorations(false)
                    .with_window_icon(Some(app_icon()))
                    .with_inner_size(winit::dpi::LogicalSize::new(1180, 800))
                    .with_min_inner_size(winit::dpi::LogicalSize::new(760, 520)),
            )
            .expect("Create app window");
        round_window(&window);
        // Preserve the previous player's browser profile, without reading or exporting cookies.
        let profile = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .expect("Local app data")
            .join("Tinysound")
            .join("WebProfileV3");
        let mut context = WebContext::new(Some(profile));
        let proxy = self.proxy.clone();
        let shell_result = WebViewBuilder::new_with_web_context(&mut context)
            .with_html(include_str!("shell.html"))
            .with_browser_extensions_enabled(true)
            .with_devtools(false)
            .with_navigation_handler(|url| shell_navigation_allowed(&url))
            .with_new_window_req_handler(|_, _| NewWindowResponse::Deny)
            .with_ipc_handler(move |request| {
                // Only the static local chrome has a host bridge; remote music pages never do.
                if request.uri().to_string() == "about:blank" && command_allowed(request.body()) {
                    let _ = proxy.send_event(Event::Command(request.body().clone()));
                }
            })
            .build(&window);
        let shell = match shell_result {
            Ok(view) => view,
            Err(e) => {
                show_message(&format!(
                    "Could not start the app frame. Update Microsoft Edge WebView2 Runtime.\n{e}"
                ));
                event_loop.exit();
                return;
            }
        };
        // Native hit testing makes CSS drag regions behave like a real Windows title bar.
        // This avoids the delayed JavaScript-to-host mouse capture that prevented dragging.
        let native_drag = unsafe {
            shell
                .webview()
                .Settings()
                .and_then(|settings| settings.cast::<ICoreWebView2Settings9>())
                .and_then(|settings| settings.SetIsNonClientRegionSupportEnabled(true))
        };
        if native_drag.is_err() {
            // Older runtimes retain a usable native title bar instead of trapping the window.
            window.set_decorations(true);
        }
        let proxy = self.proxy.clone();
        let player_result = WebViewBuilder::new_with_web_context(&mut context)
            .with_environment(shell.environment())
            .with_browser_extensions_enabled(true)
            .with_url(self.service.url())
            .with_devtools(false)
            .with_bounds(player_bounds(&window))
            .with_new_window_req_handler(|_, _| NewWindowResponse::Allow)
            .with_on_page_load_handler(move |event, _| {
                if matches!(event, PageLoadEvent::Finished) {
                    let _ = proxy.send_event(Event::Loaded);
                }
            })
            .build_as_child(&window);
        match player_result {
            Ok(player) => {
                if let Err(error) = install_blocker(&player, extension_path(), self.proxy.clone()) {
                    let _ = self
                        .proxy
                        .send_event(Event::Extension(Err(error.to_string())));
                }
                self.player = Some(player);
            }
            Err(error) => {
                show_message(&format!("Could not start the music player:\n{error}"));
                event_loop.exit();
                return;
            }
        }
        self.shell = Some(shell);
        self.window = Some(window);
        self.context = Some(context);
        self.update_shell();
    }
    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: Event) {
        match event {
            Event::Routing(routing::Notice::Devices(devices)) => {
                self.output_devices = devices;
                self.output_index = None;
                self.update_shell();
            }
            Event::Routing(routing::Notice::Status(status)) => {
                self.output_status = status;
                self.update_shell();
            }
            Event::Discord(discord::Notice::Status(enabled, status)) => {
                self.discord_enabled = enabled;
                self.discord_status = status;
                self.update_shell();
            }
            Event::Discord(discord::Notice::Tick) => {
                if !self.discord_enabled {
                    return;
                }
                if self.service == Service::Spotify || self.extension_page {
                    self.discord.update(None);
                    return;
                }
                if let Some(player) = &self.player {
                    let url = player.url().unwrap_or_default();
                    if !(url.starts_with("https://soundcloud.com/")
                        || url.starts_with("https://music.youtube.com/"))
                    {
                        self.discord.update(None);
                        return;
                    }
                    let mut playing = BOOL(0);
                    let result = unsafe {
                        player
                            .webview()
                            .cast::<ICoreWebView2_8>()
                            .and_then(|v| v.IsDocumentPlayingAudio(&mut playing))
                    };
                    if result.is_err() || !playing.as_bool() {
                        self.discord.update(None);
                        return;
                    }
                    let proxy = self.proxy.clone();
                    let service = self.service;
                    let _=player.evaluate_script_with_callback(r#"(()=>{const m=navigator.mediaSession?.metadata;const art=Array.from(m?.artwork||[]).filter(a=>a.src?.startsWith('https://')).sort((a,b)=>(parseInt(b.sizes)||0)-(parseInt(a.sizes)||0));return {title:m?.title||'',artist:m?.artist||'',artwork:art[0]?.src||''}})()"#,move |json|{let _=proxy.send_event(Event::Metadata(service,json));});
                }
            }
            Event::Metadata(service, json) => {
                if self.discord_enabled
                    && service == self.service
                    && service != Service::Spotify
                    && !self.extension_page
                {
                    let value: serde_json::Value = serde_json::from_str(&json).unwrap_or_default();
                    self.discord.update(Some(discord::activity(
                        service.name(),
                        value["title"].as_str().unwrap_or(""),
                        value["artist"].as_str().unwrap_or(""),
                        value["artwork"].as_str().unwrap_or(""),
                    )));
                }
            }
            Event::Audio(mode, status) => {
                self.audio_mode = mode;
                self.audio_status = status;
                self.update_shell();
            }
            Event::Extension(result) => {
                self.extension_id = result.ok();
                self.update_shell();
            }
            Event::Loaded => self.update_shell(),
            Event::Command(command) => match command.as_str() {
                "output-refresh" => self.routing.refresh(),
                cmd if cmd.starts_with("output-device:") => {
                    if let Ok(index) = cmd[14..].parse::<usize>() {
                        if index < self.output_devices.len() {
                            self.output_index = Some(index);
                            self.output_status = "Applying app-only output…".into();
                            self.routing.select(index);
                            self.update_shell();
                        }
                    }
                }
                "discord-toggle" => {
                    let enabled = !self.discord_enabled;
                    self.discord_enabled = enabled;
                    self.discord_status = if enabled {
                        "Connecting to Discord…"
                    } else {
                        "Disconnecting…"
                    }
                    .into();
                    self.discord.enable(enabled);
                    self.update_shell();
                }
                "audio-stereo" | "audio-left" | "audio-right" => {
                    let mode = match command.as_str() {
                        "audio-left" => audio::Mode::Left,
                        "audio-right" => audio::Mode::Right,
                        _ => audio::Mode::Stereo,
                    };
                    self.audio.set(mode);
                }
                "ready" => self.update_shell(),
                "soundcloud" => self.switch(Service::SoundCloud),
                "spotify" => self.switch(Service::Spotify),
                "youtube" => self.switch(Service::YouTubeMusic),
                "return" => self.show_player(),
                "settings" => {
                    self.settings = !self.settings;
                    if let Some(player) = &self.player {
                        let _ = player.set_visible(!self.settings);
                    }
                    self.update_shell();
                }
                "blocker" => {
                    if let (Some(id), Some(player)) = (&self.extension_id, &self.player) {
                        if player
                            .load_url(&format!("chrome-extension://{id}/dashboard.html"))
                            .is_ok()
                        {
                            self.extension_page = true;
                            self.show_player();
                        }
                    } else {
                        show_message(
                            "The external ad blocker is unavailable. Keep the extensions folder beside the app, and use an up-to-date WebView2 Runtime.",
                        );
                    }
                }
                "reload" => {
                    if let Some(player) = &self.player {
                        let _ = player.reload();
                    }
                }
                "back" => {
                    if let Some(player) = &self.player {
                        unsafe {
                            let _ = player.webview().GoBack();
                        }
                    }
                }
                "close" => event_loop.exit(),
                "minimize" => {
                    if let Some(window) = &self.window {
                        window.set_minimized(true);
                    }
                }
                "maximize" => {
                    if let Some(window) = &self.window {
                        window.set_maximized(!window.is_maximized());
                    }
                }
                "drag" => {
                    if let Some(window) = &self.window {
                        let _ = window.drag_window();
                    }
                }
                cmd if cmd.starts_with("resize-") => {
                    let direction = match cmd {
                        "resize-n" => ResizeDirection::North,
                        "resize-s" => ResizeDirection::South,
                        "resize-e" => ResizeDirection::East,
                        "resize-w" => ResizeDirection::West,
                        "resize-ne" => ResizeDirection::NorthEast,
                        "resize-nw" => ResizeDirection::NorthWest,
                        "resize-se" => ResizeDirection::SouthEast,
                        _ => ResizeDirection::SouthWest,
                    };
                    if let Some(window) = &self.window {
                        let _ = window.drag_resize_window(direction);
                    }
                }
                _ => {}
            },
        }
    }
    fn window_event(&mut self, event_loop: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. } => self.resize(),
            WindowEvent::Focused(focused) => {
                for view in [&self.player, &self.shell].into_iter().flatten() {
                    let _ = view.set_memory_usage_level(if focused {
                        MemoryUsageLevel::Normal
                    } else {
                        MemoryUsageLevel::Low
                    });
                }
            }
            _ => {}
        }
    }
}
fn extension_path() -> PathBuf {
    std::env::current_exe()
        .unwrap_or_default()
        .parent()
        .unwrap_or(std::path::Path::new("."))
        .join("extensions")
        .join("ublock-lite")
}

fn install_blocker(
    view: &WebView,
    folder: PathBuf,
    proxy: EventLoopProxy<Event>,
) -> windows::core::Result<()> {
    unsafe {
        let profile = view
            .webview()
            .cast::<ICoreWebView2_13>()?
            .Profile()?
            .cast::<ICoreWebView2Profile7>()?;
        let handler = ProfileAddBrowserExtensionCompletedHandler::create(Box::new(
            move |result, extension| {
                let outcome = (|| -> Result<String, String> {
                    result.map_err(|e| e.to_string())?;
                    let extension = extension.ok_or("Extension was not returned")?;
                    let mut enabled = BOOL(0);
                    extension
                        .IsEnabled(&mut enabled)
                        .map_err(|e| e.to_string())?;
                    if !enabled.as_bool() {
                        return Err("Extension is disabled".into());
                    }
                    let mut id = PWSTR::null();
                    extension.Id(&mut id).map_err(|e| e.to_string())?;
                    Ok(take_pwstr(id))
                })();
                let _ = proxy.send_event(Event::Extension(outcome));
                Ok(())
            },
        ));
        profile.AddBrowserExtension(&HSTRING::from(folder.as_os_str()), &handler)?;
    }
    Ok(())
}

fn show_message(message: &str) {
    #[link(name = "user32")]
    unsafe extern "system" {
        fn MessageBoxW(hwnd: isize, text: *const u16, caption: *const u16, kind: u32) -> i32;
    }
    let text: Vec<u16> = message.encode_utf16().chain(Some(0)).collect();
    let title: Vec<u16> = "Tinysound".encode_utf16().chain(Some(0)).collect();
    unsafe {
        MessageBoxW(0, text.as_ptr(), title.as_ptr(), 0x40);
    }
}
fn round_window(window: &Window) {
    #[link(name = "gdi32")]
    unsafe extern "system" {
        fn CreateRoundRectRgn(
            left: i32,
            top: i32,
            right: i32,
            bottom: i32,
            width: i32,
            height: i32,
        ) -> isize;
        fn DeleteObject(object: isize) -> i32;
    }
    #[link(name = "user32")]
    unsafe extern "system" {
        fn SetWindowRgn(window: isize, region: isize, redraw: i32) -> i32;
    }
    if let Ok(handle) = window.window_handle() {
        if let RawWindowHandle::Win32(handle) = handle.as_raw() {
            unsafe {
                if window.is_maximized() {
                    SetWindowRgn(handle.hwnd.get(), 0, 1);
                    return;
                }
                let size = window.inner_size();
                let diameter = (28.0 * window.scale_factor()).round() as i32;
                let region = CreateRoundRectRgn(
                    0,
                    0,
                    size.width as i32 + 1,
                    size.height as i32 + 1,
                    diameter,
                    diameter,
                );
                // Windows owns the region after a successful assignment.
                if region != 0 && SetWindowRgn(handle.hwnd.get(), region, 1) == 0 {
                    DeleteObject(region);
                }
            }
        }
    }
}

fn app_icon() -> Icon {
    let mut pixels = vec![0; 64 * 64 * 4];
    for y in 0..64 {
        for x in 0..64 {
            let i = (y * 64 + x) * 4;
            pixels[i..i + 4].copy_from_slice(&[26, 24, 37, 255]);
        }
    }
    for (bar, height) in [14, 28, 42, 28, 14].iter().enumerate() {
        for y in (64 - height) / 2..(64 + height) / 2 {
            for x in 13 + bar * 8..17 + bar * 8 {
                let i = (y * 64 + x) * 4;
                pixels[i..i + 4].copy_from_slice(&[204, 187, 255, 255]);
            }
        }
    }
    Icon::from_rgba(pixels, 64, 64).unwrap()
}
fn main() {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateMutexW(attributes: *const std::ffi::c_void, owner: i32, name: *const u16)
        -> isize;
        fn GetLastError() -> u32;
    }
    #[link(name = "user32")]
    unsafe extern "system" {
        fn FindWindowW(class: *const u16, title: *const u16) -> isize;
        fn ShowWindow(window: isize, command: i32) -> i32;
        fn SetForegroundWindow(window: isize) -> i32;
    }
    let name: Vec<u16> = "Local\\Tinysound.0.4"
        .encode_utf16()
        .chain(Some(0))
        .collect();
    unsafe {
        if CreateMutexW(std::ptr::null(), 0, name.as_ptr()) == 0 {
            show_message("Unable to initialize Tinysound.");
            return;
        }
        if GetLastError() == 183 {
            let title: Vec<u16> = "Tinysound".encode_utf16().chain(Some(0)).collect();
            let window = FindWindowW(std::ptr::null(), title.as_ptr());
            if window != 0 {
                ShowWindow(window, 9);
                SetForegroundWindow(window);
            }
            return;
        }
    }
    let event_loop = EventLoop::<Event>::with_user_event()
        .build()
        .expect("Windows event loop");
    event_loop.set_control_flow(ControlFlow::Wait);
    let proxy = event_loop.create_proxy();
    let routing_proxy = proxy.clone();
    let routing = routing::Router::new(move |notice| {
        let _ = routing_proxy.send_event(Event::Routing(notice));
    });
    let discord_proxy = proxy.clone();
    let discord = discord::Presence::new(move |notice| {
        let _ = discord_proxy.send_event(Event::Discord(notice));
    });
    let audio_proxy = proxy.clone();
    let audio = audio::Controller::new(move |mode, status| {
        let _ = audio_proxy.send_event(Event::Audio(mode, status));
    });
    let mut app = App {
        routing,
        output_devices: Vec::new(),
        output_index: None,
        output_status: "Choose a device to change this app's output".into(),
        discord,
        discord_enabled: false,
        discord_status: "Discord sharing off".into(),
        audio,
        audio_mode: audio::Mode::Stereo,
        audio_status: "Stereo · original channel balance".into(),
        player: None,
        shell: None,
        window: None,
        context: None,
        proxy,
        service: Service::SoundCloud,
        extension_id: None,
        settings: false,
        extension_page: false,
    };
    if let Err(error) = event_loop.run_app(&mut app) {
        show_message(&error.to_string());
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shell_navigation_allows_only_bundled_content() {
        let encoded =
            base64::engine::general_purpose::STANDARD.encode(include_bytes!("shell.html"));
        assert!(shell_navigation_allowed("about:blank"));
        for prefix in [
            "data:text/html;base64,",
            "data:text/html;charset=utf-8;base64,",
        ] {
            assert!(shell_navigation_allowed(&format!("{prefix}{encoded}")));
        }
        assert!(!shell_navigation_allowed("https://example.com/"));
        assert!(!shell_navigation_allowed(
            "data:text/html;base64,PGgxPmV2aWw8L2gxPg=="
        ));
        assert!(!shell_navigation_allowed(
            "data:text/html;base64,not-base64"
        ));
    }
    #[test]
    fn service_destinations() {
        assert_eq!(Service::SoundCloud.url(), "https://soundcloud.com/discover");
        assert_eq!(Service::Spotify.url(), "https://open.spotify.com/");
        assert_eq!(Service::YouTubeMusic.url(), "https://music.youtube.com/");
        assert_eq!(Service::YouTubeMusic.key(), "youtube");
        assert_eq!(Service::YouTubeMusic.name(), "YouTube Music");
    }
    #[test]
    fn bridge_rejects_arbitrary_actions() {
        for bad in [
            "https://evil.example",
            "eval",
            "exec",
            "resize-invalid",
            "open-file",
        ] {
            assert!(!command_allowed(bad));
        }
        assert!(command_allowed("spotify"));
        assert!(command_allowed("youtube"));
        assert!(command_allowed("close"));
    }
    #[test]
    fn icon_is_valid() {
        let _ = app_icon();
    }
}
