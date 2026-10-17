use std::{
    collections::{HashMap, HashSet},
    sync::mpsc,
    thread,
    time::Duration,
};
use windows::{
    Win32::{
        Foundation::CloseHandle,
        Media::Audio::*,
        System::{Com::*, Diagnostics::ToolHelp::*},
    },
    core::Interface,
};

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Mode {
    Stereo,
    Left,
    Right,
}
enum Command {
    Set(Mode),
    Stop,
}
pub struct Controller {
    sender: mpsc::Sender<Command>,
    worker: Option<thread::JoinHandle<()>>,
}
impl Controller {
    pub fn new(report: impl Fn(Mode, String) + Send + 'static) -> Self {
        let (sender, receiver) = mpsc::channel();
        let worker = thread::spawn(move || unsafe {
            if CoInitializeEx(None, COINIT_MULTITHREADED).is_err() {
                report(Mode::Stereo, "Windows audio initialization failed".into());
                return;
            }
            let mut saved = HashMap::<String, (IChannelAudioVolume, Vec<f32>)>::new();
            let mut mode = Mode::Stereo;
            let mut previous = String::new();
            loop {
                let command = if mode == Mode::Stereo {
                    receiver
                        .recv()
                        .map_err(|_| mpsc::RecvTimeoutError::Disconnected)
                } else {
                    receiver.recv_timeout(Duration::from_secs(2))
                };
                match command {
                    Ok(Command::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    Ok(Command::Set(next)) => {
                        mode = next;
                        previous.clear();
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                }
                let status = if mode == Mode::Stereo {
                    let mut failed = false;
                    for (channel, original) in saved.values() {
                        failed |= channel.SetAllVolumes(original, std::ptr::null()).is_err();
                    }
                    if failed {
                        "Some previous audio sessions have ended".into()
                    } else {
                        "Stereo · original channel balance restored".into()
                    }
                } else {
                    match apply(mode, &mut saved) {
                        Ok(0) => "Waiting for this app's stereo playback…".into(),
                        Ok(_) => match mode {
                            Mode::Left => "Left ear only · this app",
                            _ => "Right ear only · this app",
                        }
                        .into(),
                        Err(error) => format!("Audio control unavailable: {error}"),
                    }
                };
                if status != previous {
                    report(mode, status.clone());
                    previous = status;
                }
            }
            for (channel, original) in saved.values() {
                let _ = channel.SetAllVolumes(original, std::ptr::null());
            }
            drop(saved);
            CoUninitialize();
        });
        Self {
            sender,
            worker: Some(worker),
        }
    }
    pub fn set(&self, mode: Mode) {
        let _ = self.sender.send(Command::Set(mode));
    }
}
impl Drop for Controller {
    fn drop(&mut self) {
        let _ = self.sender.send(Command::Stop);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn descendants(root: u32, processes: &[(u32, u32)]) -> HashSet<u32> {
    let mut owned = HashSet::from([root]);
    loop {
        let before = owned.len();
        for &(pid, parent) in processes {
            if owned.contains(&parent) {
                owned.insert(pid);
            }
        }
        if owned.len() == before {
            return owned;
        }
    }
}
fn channel_levels(mode: Mode, baseline: &[f32]) -> Vec<f32> {
    if mode == Mode::Stereo {
        return baseline.to_vec();
    }
    let mut levels = vec![0.0; baseline.len()];
    let selected = if mode == Mode::Left { 0 } else { 1 };
    if selected < baseline.len() {
        levels[selected] = baseline[selected];
    }
    levels
}
unsafe fn apply(
    mode: Mode,
    saved: &mut HashMap<String, (IChannelAudioVolume, Vec<f32>)>,
) -> windows::core::Result<usize> {
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)?;
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut processes = Vec::new();
        if Process32FirstW(snapshot, &mut entry).is_ok() {
            loop {
                processes.push((entry.th32ProcessID, entry.th32ParentProcessID));
                if Process32NextW(snapshot, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snapshot);
        let owned = descendants(std::process::id(), &processes);
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let devices = enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)?;
        let mut applied = 0;
        for i in 0..devices.GetCount()? {
            let device = devices.Item(i)?;
            let manager: IAudioSessionManager2 = device.Activate(CLSCTX_ALL, None)?;
            let sessions = manager.GetSessionEnumerator()?;
            for j in 0..sessions.GetCount()? {
                let session = sessions.GetSession(j)?;
                let control: IAudioSessionControl2 = session.cast()?;
                // Never touch another app, system sounds, endpoint volume, or default routing.
                if !owned.contains(&control.GetProcessId()?) {
                    continue;
                }
                let channel: IChannelAudioVolume = session.cast()?;
                let count = channel.GetChannelCount()?;
                if !(2..=32).contains(&count) {
                    continue;
                }
                let raw = control.GetSessionInstanceIdentifier()?;
                let key = raw.to_string()?;
                CoTaskMemFree(Some(raw.0.cast()));
                if !saved.contains_key(&key) {
                    let baseline = (0..count)
                        .map(|index| channel.GetChannelVolume(index))
                        .collect::<windows::core::Result<Vec<_>>>()?;
                    saved.insert(key.clone(), (channel.clone(), baseline));
                }
                channel.SetAllVolumes(&channel_levels(mode, &saved[&key].1), std::ptr::null())?;
                applied += 1;
            }
        }
        Ok(applied)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_session_left_right_and_restore() {
        // Create a silent, unstarted session owned by this test process, never by another app.
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED).ok().unwrap();
            {
                let enumerator: IMMDeviceEnumerator =
                    CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).unwrap();
                let device = enumerator
                    .GetDefaultAudioEndpoint(eRender, eMultimedia)
                    .unwrap();
                let client: IAudioClient = device.Activate(CLSCTX_ALL, None).unwrap();
                let format = WAVEFORMATEX {
                    wFormatTag: 3,
                    nChannels: 2,
                    nSamplesPerSec: 48000,
                    nAvgBytesPerSec: 384000,
                    nBlockAlign: 8,
                    wBitsPerSample: 32,
                    cbSize: 0,
                };
                client
                    .Initialize(
                        AUDCLNT_SHAREMODE_SHARED,
                        AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
                            | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                        1_000_000,
                        0,
                        &format,
                        None,
                    )
                    .unwrap();
                let session: IAudioSessionControl = client.GetService().unwrap();
                let channel: IChannelAudioVolume = session.cast().unwrap();
                channel
                    .SetAllVolumes(&[0.7, 0.8], std::ptr::null())
                    .unwrap();
                let mut saved = HashMap::new();
                assert!(apply(Mode::Left, &mut saved).unwrap() > 0);
                assert!((channel.GetChannelVolume(0).unwrap() - 0.7).abs() < 0.001);
                assert_eq!(channel.GetChannelVolume(1).unwrap(), 0.0);
                assert!(apply(Mode::Right, &mut saved).unwrap() > 0);
                assert_eq!(channel.GetChannelVolume(0).unwrap(), 0.0);
                assert!((channel.GetChannelVolume(1).unwrap() - 0.8).abs() < 0.001);
                for (control, baseline) in saved.values() {
                    control.SetAllVolumes(baseline, std::ptr::null()).unwrap();
                }
                assert!((channel.GetChannelVolume(0).unwrap() - 0.7).abs() < 0.001);
                assert!((channel.GetChannelVolume(1).unwrap() - 0.8).abs() < 0.001);
            }
            CoUninitialize();
        }
    }
    #[test]
    fn unrelated_processes_are_excluded() {
        assert_eq!(
            descendants(10, &[(20, 10), (30, 20), (99, 1)]),
            HashSet::from([10, 20, 30])
        );
    }
    #[test]
    fn multichannel_virtual_output_keeps_only_selected_front_channel() {
        let baseline = vec![0.8; 8];
        assert_eq!(
            channel_levels(Mode::Left, &baseline),
            vec![0.8, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]
        );
        assert_eq!(
            channel_levels(Mode::Right, &baseline),
            vec![0.0, 0.8, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]
        );
        assert_eq!(channel_levels(Mode::Stereo, &baseline), baseline);
    }
    #[test]
    fn channel_modes_preserve_selected_channel() {
        assert_eq!(channel_levels(Mode::Left, &[0.7, 0.8]), vec![0.7, 0.0]);
        assert_eq!(channel_levels(Mode::Right, &[0.7, 0.8]), vec![0.0, 0.8]);
        assert_eq!(channel_levels(Mode::Stereo, &[0.7, 0.8]), vec![0.7, 0.8]);
    }
}
