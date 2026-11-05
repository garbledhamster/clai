//! Linux launch argument input, separate from the ordered sandbox policy builder.
use std::ffi::OsString;

// Flatpak disallows nested user namespaces. Its existing host route preserves
// the same bwrap policy and requires org.freedesktop.Flatpak talk permission.
pub(super) fn launch_argv(
    mut args: Vec<OsString>,
    in_flatpak: bool,
    argument_fd: Option<i32>,
) -> (&'static str, Vec<OsString>) {
    if let Some(fd) = argument_fd {
        args = vec!["--args".into(), fd.to_string().into()];
    }
    if in_flatpak {
        let mut launch = vec!["--host".into()];
        if let Some(fd) = argument_fd {
            launch.push(format!("--forward-fd={fd}").into());
        }
        launch.push("bwrap".into());
        launch.extend(args);
        ("flatpak-spawn", launch)
    } else {
        ("bwrap", args)
    }
}

fn encode_args(args: &[OsString]) -> Result<Vec<u8>, String> {
    use std::os::unix::ffi::OsStrExt;
    let mut bytes = Vec::new();
    for arg in args {
        if arg.as_bytes().contains(&0) {
            return Err("Sandbox argument contains a NUL byte".into());
        }
        bytes.extend_from_slice(arg.as_bytes());
        bytes.push(0);
    }
    Ok(bytes)
}

/// Seekable anonymous memory avoids pipe-capacity deadlocks before spawn.
/// CLOEXEC remains set in the parent, including while other processes spawn.
#[cfg(target_os = "linux")]
pub(super) struct ArgumentInput(std::fs::File);

#[cfg(target_os = "linux")]
impl ArgumentInput {
    pub(super) fn new(args: &[OsString]) -> Result<Self, String> {
        use std::io::{Seek, SeekFrom, Write};
        use std::os::fd::{AsRawFd, FromRawFd};
        let bytes = encode_args(args)?;
        let fd = unsafe {
            libc::memfd_create(
                c"clai-sandbox-args".as_ptr(),
                libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
            )
        };
        if fd < 0 {
            return Err("Variables execution is unavailable: anonymous sandbox argument input could not be created".into());
        }
        let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
        file.write_all(&bytes).and_then(|_| file.seek(SeekFrom::Start(0)).map(|_| ()))
            .map_err(|_| "Variables execution is unavailable: anonymous sandbox argument input could not be prepared")?;
        let sealed = unsafe {
            libc::fcntl(
                file.as_raw_fd(),
                libc::F_ADD_SEALS,
                libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL,
            )
        };
        if sealed < 0 {
            return Err("Variables execution is unavailable: anonymous sandbox argument input could not be sealed".into());
        }
        Ok(Self(file))
    }

    pub(super) fn fd(&self) -> i32 {
        use std::os::fd::AsRawFd;
        self.0.as_raw_fd()
    }

    pub(super) fn forward_to(&self, command: &mut tokio::process::Command) {
        let fd = self.fd();
        // Only async-signal-safe fcntl calls run after fork. The parent keeps
        // CLOEXEC, so unrelated concurrent children cannot inherit this input.
        unsafe {
            command.pre_exec(move || {
                let flags = libc::fcntl(fd, libc::F_GETFD);
                if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anonymous_argument_input_keeps_values_out_of_native_launcher_argv() {
        let args = [
            "--setenv",
            "CLAI_VAR_TOKEN",
            "disposable-secret",
            "--",
            "/bin/true",
        ]
        .map(OsString::from)
        .to_vec();
        let (program, argv) = launch_argv(args, false, Some(7));
        assert_eq!(program, "bwrap");
        assert!(
            !argv.iter().any(|arg| arg == "disposable-secret"),
            "payload must not be a launcher argument"
        );
        assert_eq!(argv, ["--args", "7"].map(OsString::from));
    }

    #[test]
    fn flatpak_forwards_the_argument_descriptor_without_values_in_argv() {
        let (program, argv) = launch_argv(vec!["disposable-secret".into()], true, Some(9));
        assert_eq!(program, "flatpak-spawn");
        assert!(!argv.iter().any(|arg| arg == "disposable-secret"));
        assert_eq!(
            argv,
            ["--host", "--forward-fd=9", "bwrap", "--args", "9"].map(OsString::from)
        );
    }

    #[test]
    fn descriptor_encoding_preserves_bytes_and_rejects_nul_argument_injection() {
        use std::os::unix::ffi::OsStringExt;
        let args = vec![
            OsString::from("first"),
            OsString::from_vec(vec![b'x', 0xff]),
        ];
        assert_eq!(
            encode_args(&args).unwrap(),
            vec![b'f', b'i', b'r', b's', b't', 0, b'x', 0xff, 0]
        );
        assert!(
            encode_args(&[OsString::from_vec(b"safe\0--bind".to_vec())]).is_err(),
            "NUL must not turn one approved argument into several"
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn anonymous_input_is_parent_private_sealed_and_closed_after_use_or_spawn_failure() {
        use std::os::fd::AsRawFd;
        let payload = "x".repeat(96 * 1024);
        let input = ArgumentInput::new(&[OsString::from(&payload)]).unwrap();
        let fd = input.fd();
        let descriptor_path = format!("/proc/self/fd/{fd}");
        let unrelated = tokio::process::Command::new("/bin/cat")
            .arg(&descriptor_path)
            .output()
            .await
            .unwrap();
        assert!(
            !unrelated.status.success(),
            "unrelated children must not inherit argument input"
        );
        let mut reader = tokio::process::Command::new("/bin/cat");
        reader.arg(&descriptor_path);
        input.forward_to(&mut reader);
        let output = reader.output().await.unwrap();
        assert!(output.status.success());
        assert_eq!(
            output.stdout.len(),
            96 * 1024 + 1,
            "input larger than pipe capacity must arrive without blocking"
        );
        assert_eq!(&output.stdout[..96 * 1024], payload.as_bytes());
        assert_eq!(output.stdout[96 * 1024], 0);
        assert_eq!(
            unsafe { libc::write(input.0.as_raw_fd(), b"a".as_ptr().cast(), 1) },
            -1,
            "sealed input must not be writable by the receiver"
        );
        drop(input);
        assert_eq!(unsafe { libc::fcntl(fd, libc::F_GETFD) }, -1);

        let input = ArgumentInput::new(&["unused".into()]).unwrap();
        let fd = input.fd();
        let mut missing = tokio::process::Command::new("/missing-clai-qa-launcher");
        input.forward_to(&mut missing);
        assert!(missing.spawn().is_err());
        drop(input);
        assert_eq!(unsafe { libc::fcntl(fd, libc::F_GETFD) }, -1);
    }
}
