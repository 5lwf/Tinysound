// Per-app Windows audio policy ABI reference: File-New-Project/EarTrumpet (MIT).
// This internal Windows interface may be unavailable on future OS versions; errors are surfaced.
use std::{collections::HashSet, ffi::c_void, sync::mpsc, thread, time::Duration};
use windows::{
    Win32::{
        Foundation::{CloseHandle, PROPERTYKEY},
        Media::Audio::*,
        System::{
            Com::{StructuredStorage::*, *},
            Diagnostics::ToolHelp::*,
        },
    },
    core::{GUID, HRESULT, HSTRING, Interface},
};
pub enum Notice {
    Devices(Vec<String>),
    Status(String),
}
enum Command {
    Select(usize),
    Refresh,
    Stop,
}
pub struct Router {
    tx: mpsc::Sender<Command>,
    worker: Option<thread::JoinHandle<()>>,
}
struct Policy(*mut c_void);
impl Policy {
    unsafe fn new() -> Result<Self, String> {
        unsafe {
            #[link(name = "runtimeobject")]
            unsafe extern "system" {
                fn RoGetActivationFactory(
                    class: *mut c_void,
                    iid: *const GUID,
                    out: *mut *mut c_void,
                ) -> HRESULT;
            }
            let class = HSTRING::from("Windows.Media.Internal.AudioPolicyConfig");
            for iid in [
                GUID::from_u128(0xab3d4648_e242_459f_b02f_541c70306324),
                GUID::from_u128(0x2a59116d_6c4f_45e0_a74f_707e3fef9258),
            ] {
                let mut object = std::ptr::null_mut();
                if RoGetActivationFactory(std::mem::transmute_copy(&class), &iid, &mut object)
                    .is_ok()
                {
                    return Ok(Self(object));
                }
            }
            Err("This Windows version does not expose per-app output routing".into())
        }
    }
    unsafe fn set(&self, pid: u32, device: Option<&str>) -> Result<(), String> {
        unsafe {
            let table = *(self.0 as *const *const usize);
            // IInspectable's 6 methods, followed by 19 preceding audio-policy methods.
            let set: unsafe extern "system" fn(*mut c_void, u32, i32, i32, *mut c_void) -> HRESULT =
                std::mem::transmute(*table.add(25));
            let get: unsafe extern "system" fn(
                *mut c_void,
                u32,
                i32,
                i32,
                *mut *mut c_void,
            ) -> HRESULT = std::mem::transmute(*table.add(26));
            let id = device.map(|id| {
                HSTRING::from(format!(
                    r"\\?\SWD#MMDEVAPI#{id}#{{e6327cad-dcec-4949-ae8a-991e976a79d2}}"
                ))
            });
            let raw = id
                .as_ref()
                .map(|id| std::mem::transmute_copy::<HSTRING, *mut c_void>(id))
                .unwrap_or(std::ptr::null_mut());
            for role in [0, 1] {
                set(self.0, pid, 0, role, raw)
                    .ok()
                    .map_err(|e| e.to_string())?;
                let mut returned = std::ptr::null_mut();
                get(self.0, pid, 0, role, &mut returned)
                    .ok()
                    .map_err(|e| e.to_string())?;
                let actual: HSTRING = std::mem::transmute(returned);
                if actual != id.clone().unwrap_or_default() {
                    return Err("Windows did not retain the selected app output".into());
                }
            }
            Ok(())
        }
    }
}
impl Drop for Policy {
    fn drop(&mut self) {
        unsafe {
            let table = *(self.0 as *const *const usize);
            let release: unsafe extern "system" fn(*mut c_void) -> u32 =
                std::mem::transmute(*table.add(2));
            release(self.0);
        }
    }
}
unsafe fn devices() -> windows::core::Result<Vec<(String, String)>> {
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let devices = enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)?;
        let mut result = vec![(String::new(), "Follow Windows default".into())];
        for i in 0..devices.GetCount()? {
            let device = devices.Item(i)?;
            let raw = device.GetId()?;
            let id = raw.to_string()?;
            CoTaskMemFree(Some(raw.0.cast()));
            let properties = device.OpenPropertyStore(STGM_READ)?;
            let mut value = properties.GetValue(&PROPERTYKEY {
                fmtid: GUID::from_u128(0xa45c254e_df1c_4efd_8020_67d146a850e0),
                pid: 14,
            })?;
            let name = PropVariantToStringAlloc(&value)?;
            let label = name.to_string().unwrap_or_else(|_| id.clone());
            CoTaskMemFree(Some(name.0.cast()));
            let _ = PropVariantClear(&mut value);
            result.push((id, label));
        }
        Ok(result)
    }
}
unsafe fn audio_processes() -> windows::core::Result<HashSet<u32>> {
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)?;
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut processes = Vec::new();
        if Process32FirstW(snap, &mut entry).is_ok() {
            loop {
                processes.push((entry.th32ProcessID, entry.th32ParentProcessID));
                if Process32NextW(snap, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
        let mut owned = HashSet::from([std::process::id()]);
        loop {
            let n = owned.len();
            for &(pid, parent) in &processes {
                if owned.contains(&parent) {
                    owned.insert(pid);
                }
            }
            if n == owned.len() {
                break;
            }
        }
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let devices = enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)?;
        let mut result = HashSet::new();
        for i in 0..devices.GetCount()? {
            let device = devices.Item(i)?;
            let manager: IAudioSessionManager2 = device.Activate(CLSCTX_ALL, None)?;
            let sessions = manager.GetSessionEnumerator()?;
            for j in 0..sessions.GetCount()? {
                let session: IAudioSessionControl2 = sessions.GetSession(j)?.cast()?;
                let pid = session.GetProcessId()?;
                if owned.contains(&pid) {
                    result.insert(pid);
                }
            }
        }
        Ok(result)
    }
}
impl Router {
    pub fn new(report: impl Fn(Notice) + Send + 'static) -> Self {
        let (tx, rx) = mpsc::channel();
        let worker = thread::spawn(move || unsafe {
            if CoInitializeEx(None, COINIT_MULTITHREADED).is_err() {
                report(Notice::Status("Audio device discovery unavailable".into()));
                return;
            }
            let mut list = devices().unwrap_or_default();
            report(Notice::Devices(list.iter().map(|v| v.1.clone()).collect()));
            let mut target: Option<String> = None;
            let mut routed = HashSet::new();
            let mut previous = String::new();
            loop {
                let cmd = if target.is_some() {
                    rx.recv_timeout(Duration::from_secs(2))
                } else {
                    rx.recv().map_err(|_| mpsc::RecvTimeoutError::Disconnected)
                };
                match cmd {
                    Ok(Command::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    Ok(Command::Refresh) => {
                        list = devices().unwrap_or_default();
                        report(Notice::Devices(list.iter().map(|v| v.1.clone()).collect()));
                        continue;
                    }
                    Ok(Command::Select(index)) => {
                        let Some((id, _)) = list.get(index) else {
                            report(Notice::Status(
                                "Device list changed. Refresh and select again.".into(),
                            ));
                            continue;
                        };
                        target = Some(id.clone());
                        routed.clear();
                        previous.clear();
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                }
                let Some(id) = &target else {
                    continue;
                };
                let result = (|| -> Result<usize, String> {
                    let active = audio_processes().map_err(|e| e.to_string())?;
                    let policy = Policy::new()?;
                    for pid in active.iter().filter(|pid| !routed.contains(*pid)) {
                        policy.set(*pid, if id.is_empty() { None } else { Some(id) })?;
                    }
                    routed.extend(active.iter().copied());
                    Ok(active.len())
                })();
                let status = match result {
                    Ok(0) => "Output selected · start playback to apply it".into(),
                    Ok(_) => "Output route applied to this app · restart playback if needed".into(),
                    Err(e) => format!("Output change failed: {e}"),
                };
                if status != previous {
                    report(Notice::Status(status.clone()));
                    previous = status;
                }
            }
            CoUninitialize();
        });
        Self {
            tx,
            worker: Some(worker),
        }
    }
    pub fn select(&self, index: usize) {
        let _ = self.tx.send(Command::Select(index));
    }
    pub fn refresh(&self) {
        let _ = self.tx.send(Command::Refresh);
    }
}
impl Drop for Router {
    fn drop(&mut self) {
        let _ = self.tx.send(Command::Stop);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn enumerate_outputs_and_policy_without_changes() {
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED).ok().unwrap();
            assert!(!devices().unwrap().is_empty());
            let policy = Policy::new().expect("per-app Windows output interface");
            drop(policy);
            CoUninitialize();
        }
    }
}
