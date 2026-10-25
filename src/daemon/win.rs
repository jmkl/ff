use std::ffi::c_void;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use windows_sys::Win32::Storage::FileSystem::GetDriveTypeW;
use windows_sys::Win32::System::IO::DeviceIoControl;

/// Raw volume handle (needs admin).
pub fn open_volume(letter: char) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .share_mode(3) // FILE_SHARE_READ | FILE_SHARE_WRITE
        .open(format!(r"\\.\{letter}:"))
}

pub fn ioctl(f: &File, code: u32, inp: &[u8], out: &mut [u8]) -> io::Result<usize> {
    let mut got = 0u32;
    let ok = unsafe {
        DeviceIoControl(
            f.as_raw_handle() as _,
            code,
            if inp.is_empty() { std::ptr::null() } else { inp.as_ptr() as *const c_void },
            inp.len() as u32,
            out.as_mut_ptr() as *mut c_void,
            out.len() as u32,
            &mut got,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(got as usize)
    }
}

/// Fixed + removable drive letters.
pub fn drive_letters() -> Vec<char> {
    ('A'..='Z')
        .filter(|c| {
            let w: Vec<u16> = format!("{c}:\\").encode_utf16().chain(Some(0)).collect();
            let t = unsafe { GetDriveTypeW(w.as_ptr()) };
            t == 2 || t == 3
        })
        .collect()
}

/// Can we read raw volumes? (i.e. are we running as Administrator)
pub fn is_elevated() -> bool {
    drive_letters()
        .first()
        .map(|&c| open_volume(c).is_ok())
        .unwrap_or(false)
}
