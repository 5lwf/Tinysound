use serde_json::{Value, json};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    os::windows::io::AsRawHandle,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};
const APPLICATION_ID: &str = "1505487506445963324";
pub enum Notice {
    Status(bool, String),
    Tick,
}
enum Command {
    Enable(bool),
    Activity(Option<Value>),
    Stop,
}
pub struct Presence {
    tx: mpsc::Sender<Command>,
    worker: Option<thread::JoinHandle<()>>,
}
struct Connection {
    pipe: File,
    sequence: u64,
}
impl Connection {
    fn connect() -> Result<Self, String> {
        for index in 0..10 {
            let path = format!(r"\\.\pipe\discord-ipc-{index}");
            if let Ok(pipe) = OpenOptions::new().read(true).write(true).open(path) {
                let mut connection = Self { pipe, sequence: 0 };
                connection.send(0, &json!({"v":1,"client_id":APPLICATION_ID}))?;
                let (opcode, reply) =
                    connection.receive(Instant::now() + Duration::from_secs(3))?;
                if opcode != 1 || reply["evt"] != "READY" {
                    return Err("Discord rejected the Application ID or connection".into());
                }
                return Ok(connection);
            }
        }
        Err("Open the Discord desktop app, then try again".into())
    }
    fn send(&mut self, opcode: u32, value: &Value) -> Result<(), String> {
        let body = value.to_string();
        let mut frame = Vec::with_capacity(body.len() + 8);
        frame.extend_from_slice(&opcode.to_le_bytes());
        frame.extend_from_slice(&(body.len() as u32).to_le_bytes());
        frame.extend_from_slice(body.as_bytes());
        self.pipe.write_all(&frame).map_err(|e| e.to_string())
    }
    fn read(&mut self, buffer: &mut [u8], deadline: Instant) -> Result<(), String> {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn PeekNamedPipe(
                handle: *mut std::ffi::c_void,
                buffer: *mut std::ffi::c_void,
                size: u32,
                read: *mut u32,
                available: *mut u32,
                left: *mut u32,
            ) -> i32;
        }
        let mut offset = 0;
        while offset < buffer.len() {
            if Instant::now() > deadline {
                return Err("Discord did not respond in time".into());
            }
            let mut available = 0;
            if unsafe {
                PeekNamedPipe(
                    self.pipe.as_raw_handle(),
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null_mut(),
                    &mut available,
                    std::ptr::null_mut(),
                )
            } == 0
            {
                return Err("Discord disconnected".into());
            }
            if available == 0 {
                thread::sleep(Duration::from_millis(20));
                continue;
            }
            let end = (offset + available as usize).min(buffer.len());
            self.pipe
                .read_exact(&mut buffer[offset..end])
                .map_err(|e| e.to_string())?;
            offset = end;
        }
        Ok(())
    }
    fn receive(&mut self, deadline: Instant) -> Result<(u32, Value), String> {
        loop {
            let mut header = [0u8; 8];
            self.read(&mut header, deadline)?;
            let opcode = u32::from_le_bytes(header[..4].try_into().unwrap());
            let length = u32::from_le_bytes(header[4..].try_into().unwrap()) as usize;
            if length > 65536 {
                return Err("Invalid Discord response length".into());
            }
            let mut body = vec![0; length];
            self.read(&mut body, deadline)?;
            let value: Value = serde_json::from_slice(&body).map_err(|e| e.to_string())?;
            if opcode == 3 {
                self.send(4, &value)?;
                continue;
            }
            return Ok((opcode, value));
        }
    }
    fn activity(&mut self, activity: Option<Value>) -> Result<(), String> {
        self.sequence += 1;
        let nonce = self.sequence.to_string();
        self.send(1,&json!({"cmd":"SET_ACTIVITY","args":{"pid":std::process::id(),"activity":activity},"nonce":nonce}))?;
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let (opcode, reply) = self.receive(deadline)?;
            if opcode == 2 {
                return Err("Discord closed the connection".into());
            }
            if reply["nonce"] == nonce {
                if reply["evt"] == "ERROR" {
                    return Err(reply["data"]["message"]
                        .as_str()
                        .unwrap_or("Discord rejected the activity")
                        .to_string());
                }
                return Ok(());
            }
        }
    }
}
impl Presence {
    pub fn new(report: impl Fn(Notice) + Send + 'static) -> Self {
        let (tx, rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            let mut connection: Option<Connection> = None;
            let mut last: Option<Value> = None;
            loop {
                let command = if connection.is_some() {
                    rx.recv_timeout(Duration::from_secs(5))
                } else {
                    rx.recv().map_err(|_| mpsc::RecvTimeoutError::Disconnected)
                };
                match command {
                    Ok(Command::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                        if let Some(c) = &mut connection {
                            let _ = c.activity(None);
                        }
                        break;
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => report(Notice::Tick),
                    Ok(Command::Enable(false)) => {
                        if let Some(c) = &mut connection {
                            let _ = c.activity(None);
                        }
                        connection = None;
                        last = None;
                        report(Notice::Status(false, "Discord sharing off".into()));
                    }
                    Ok(Command::Enable(true)) => match Connection::connect() {
                        Ok(c) => {
                            connection = Some(c);
                            last = None;
                            report(Notice::Status(
                                true,
                                "Discord linked · waiting for playback".into(),
                            ));
                            report(Notice::Tick);
                        }
                        Err(e) => report(Notice::Status(false, e)),
                    },
                    Ok(Command::Activity(activity)) => {
                        if activity == last {
                            continue;
                        }
                        if let Some(c) = &mut connection {
                            match c.activity(activity.clone()) {
                                Ok(()) => {
                                    last = activity;
                                    report(Notice::Status(
                                        true,
                                        if last.is_some() {
                                            "Listening presence shared"
                                        } else {
                                            "Discord linked · paused / Spotify excluded"
                                        }
                                        .into(),
                                    ));
                                }
                                Err(e) => {
                                    connection = None;
                                    last = None;
                                    report(Notice::Status(false, e));
                                }
                            }
                        }
                    }
                }
            }
        });
        Self {
            tx,
            worker: Some(worker),
        }
    }
    pub fn enable(&self, enabled: bool) {
        let _ = self.tx.send(Command::Enable(enabled));
    }
    pub fn update(&self, activity: Option<Value>) {
        let _ = self.tx.send(Command::Activity(activity));
    }
}
impl Drop for Presence {
    fn drop(&mut self) {
        let _ = self.tx.send(Command::Stop);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
pub fn activity(service: &str, title: &str, artist: &str, artwork: &str) -> Value {
    let title: String = title.chars().take(120).collect();
    let artist: String = artist.chars().take(120).collect();
    let mut value = json!({"type":2,"name":service,"details":if title.is_empty(){format!("Listening on {service}")}else{title}});
    if !artist.is_empty() {
        value["state"] = json!(artist);
    }
    // Discord fetches public artwork URLs; local/blob images cannot be shared.
    // Rebuild assets each update so a track without artwork cannot retain old art.
    if artwork.starts_with("https://")
        && artwork.len() <= 2048
        && !artwork.chars().any(char::is_whitespace)
    {
        value["assets"] = json!({"large_image":artwork,"large_text":value["details"]});
    }
    value
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn listening_activity_is_bounded() {
        let v = activity("SoundCloud", &"x".repeat(200), "Artist", "");
        assert_eq!(v["type"], 2);
        assert_eq!(v["details"].as_str().unwrap().len(), 120);
    }
    #[test]
    fn artwork_tracks_current_song_and_clears_when_missing() {
        let a = activity(
            "SoundCloud",
            "Song",
            "Artist",
            "https://i1.sndcdn.com/cover.jpg",
        );
        assert_eq!(
            a["assets"]["large_image"],
            "https://i1.sndcdn.com/cover.jpg"
        );
        assert_eq!(a["assets"]["large_text"], "Song");
        let b = activity(
            "SoundCloud",
            "Next",
            "Artist",
            "https://i1.sndcdn.com/next.jpg",
        );
        assert_ne!(a, b);
        for invalid in [
            "",
            "blob:https://soundcloud.com/image",
            "file:///cover.jpg",
            "https://bad image",
        ] {
            assert!(
                activity("SoundCloud", "Next", "Artist", invalid)
                    .get("assets")
                    .is_none()
            );
        }
    }
    #[test]
    #[ignore = "Requires the user's running Discord desktop app; handshake only"]
    fn discord_handshake() {
        let mut connection = Connection::connect().expect("Discord handshake");
        connection
            .activity(None)
            .expect("Discord acknowledged presence command");
    }
}
