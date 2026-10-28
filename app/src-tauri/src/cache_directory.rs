//! Descriptor-relative operations for disposable Unix Drive cache files.
//! Every traversed component is opened without following symbolic links.
use std::ffi::{CString, OsStr};
use std::fs::{File, Permissions};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path};

fn name(value: &OsStr) -> io::Result<CString> {
    if value.as_bytes().contains(&b'/') || value == ".." || value == "." {
        return Err(io::Error::other("Invalid cache component"));
    }
    CString::new(value.as_bytes()).map_err(io::Error::other)
}

pub(crate) fn open_child(dir: &File, child: &OsStr, flags: i32) -> io::Result<File> {
    let child = name(child)?;
    // SAFETY: the directory descriptor and NUL-terminated name are valid;
    // ownership of a successful new descriptor is transferred to File once.
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            child.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

pub(crate) fn open_directory(path: &Path, create: bool) -> io::Result<File> {
    let mut dir = File::open(if path.is_absolute() { "/" } else { "." })?;
    for component in path.components() {
        let Component::Normal(child) = component else {
            if matches!(component, Component::RootDir | Component::CurDir) {
                continue;
            }
            return Err(io::Error::other("Invalid cache directory component"));
        };
        match open_child(&dir, child, libc::O_RDONLY | libc::O_DIRECTORY) {
            Ok(next) => dir = next,
            Err(error) if create && error.kind() == io::ErrorKind::NotFound => {
                let child_name = name(child)?;
                // SAFETY: valid directory and name; mkdirat never follows the leaf.
                let result = unsafe { libc::mkdirat(dir.as_raw_fd(), child_name.as_ptr(), 0o700) };
                if result < 0 && io::Error::last_os_error().kind() != io::ErrorKind::AlreadyExists {
                    return Err(io::Error::last_os_error());
                }
                dir = open_child(&dir, child, libc::O_RDONLY | libc::O_DIRECTORY)?;
            }
            Err(error) => return Err(error),
        }
    }
    Ok(dir)
}

pub(crate) fn private_directory(path: &Path) -> io::Result<File> {
    let dir = open_directory(path, true)?;
    // Change the opened inode, never a pathname that can be swapped for a link.
    dir.set_permissions(Permissions::from_mode(0o700))?;
    Ok(dir)
}

pub(crate) fn remove_child(dir: &File, child: &OsStr) -> io::Result<()> {
    let child = name(child)?;
    // SAFETY: valid descriptor and name. No directory removal or link traversal.
    if unsafe { libc::unlinkat(dir.as_raw_fd(), child.as_ptr(), 0) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

pub(crate) fn rename_child(dir: &File, from: &OsStr, to: &OsStr) -> io::Result<()> {
    let from = name(from)?;
    let to = name(to)?;
    // SAFETY: both names belong to the same live directory descriptor.
    if unsafe { libc::renameat(dir.as_raw_fd(), from.as_ptr(), dir.as_raw_fd(), to.as_ptr()) } == 0
    {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}
