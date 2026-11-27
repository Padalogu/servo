/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::ffi::OsStr;
use std::{env, process};

#[cfg(any(
    target_os = "macos",
    all(
        not(target_os = "windows"),
        not(target_os = "ios"),
        not(target_os = "android"),
        not(target_env = "ohos"),
        not(target_arch = "arm"),
        not(target_arch = "aarch64"),
        not(target_arch = "riscv32"),
        not(target_arch = "riscv64")
    )
))]
use gaol::profile::{Operation, PathPattern, Profile};
use serde::{Deserialize, Serialize};
use servo_config::opts::Opts;
use servo_config::prefs::Preferences;

use crate::event_loop::NewScriptEventLoopProcessInfo;
use crate::serviceworker::ServiceWorkerUnprivilegedContent;

#[derive(Deserialize, Serialize)]
#[expect(clippy::large_enum_variant)]
pub enum UnprivilegedContent {
    ScriptEventLoop(NewScriptEventLoopProcessInfo),
    ServiceWorker(ServiceWorkerUnprivilegedContent),
}

impl UnprivilegedContent {
    pub fn opts(&self) -> Opts {
        match self {
            UnprivilegedContent::ScriptEventLoop(content) => content.opts.clone(),
            UnprivilegedContent::ServiceWorker(content) => content.opts.clone(),
        }
    }

    pub fn prefs(&self) -> &Preferences {
        match self {
            UnprivilegedContent::ScriptEventLoop(content) => &content.prefs,
            UnprivilegedContent::ServiceWorker(content) => &content.prefs,
        }
    }
}

/// Our content process sandbox profile on Mac. As restrictive as possible.
#[cfg(target_os = "macos")]
pub fn content_process_sandbox_profile() -> Profile {
    use std::path::PathBuf;

    use embedder_traits::resources;
    use gaol::platform;

    let mut operations = vec![
        Operation::FileReadAll(PathPattern::Literal(PathBuf::from("/dev/urandom"))),
        Operation::FileReadAll(PathPattern::Subpath(PathBuf::from("/Library/Fonts"))),
        Operation::FileReadAll(PathPattern::Subpath(PathBuf::from("/System/Library/Fonts"))),
        Operation::FileReadAll(PathPattern::Subpath(PathBuf::from(
            "/System/Library/Frameworks/ApplicationServices.framework",
        ))),
        Operation::FileReadAll(PathPattern::Subpath(PathBuf::from(
            "/System/Library/Frameworks/CoreGraphics.framework",
        ))),
        Operation::FileReadMetadata(PathPattern::Literal(PathBuf::from("/"))),
        Operation::FileReadMetadata(PathPattern::Literal(PathBuf::from("/Library"))),
        Operation::FileReadMetadata(PathPattern::Literal(PathBuf::from("/System"))),
        Operation::FileReadMetadata(PathPattern::Literal(PathBuf::from("/etc"))),
        Operation::SystemInfoRead,
        Operation::PlatformSpecific(platform::macos::Operation::MachLookup(
            b"com.apple.FontServer".to_vec(),
        )),
    ];

    operations.extend(
        resources::sandbox_access_files()
            .into_iter()
            .map(|p| Operation::FileReadAll(PathPattern::Literal(p))),
    );
    operations.extend(
        resources::sandbox_access_files_dirs()
            .into_iter()
            .map(|p| Operation::FileReadAll(PathPattern::Subpath(p))),
    );

    Profile::new(operations).expect("Failed to create sandbox profile!")
}

/// Our content process sandbox profile on Linux. As restrictive as possible.
#[cfg(all(
    not(target_os = "macos"),
    not(target_os = "windows"),
    not(target_os = "ios"),
    not(target_os = "android"),
    not(target_env = "ohos"),
    not(target_arch = "arm"),
    not(target_arch = "aarch64"),
    not(target_arch = "riscv32"),
    not(target_arch = "riscv64")
))]
pub fn content_process_sandbox_profile() -> Profile {
    use std::path::PathBuf;

    use embedder_traits::resources;

    let mut operations = vec![Operation::FileReadAll(PathPattern::Literal(PathBuf::from(
        "/dev/urandom",
    )))];

    operations.extend(
        resources::sandbox_access_files()
            .into_iter()
            .map(|p| Operation::FileReadAll(PathPattern::Literal(p))),
    );
    operations.extend(
        resources::sandbox_access_files_dirs()
            .into_iter()
            .map(|p| Operation::FileReadAll(PathPattern::Subpath(p))),
    );

    Profile::new(operations).expect("Failed to create sandbox profile!")
}

#[cfg(any(
    target_os = "windows",
    target_os = "ios",
    target_os = "android",
    target_env = "ohos",
    target_arch = "arm",
    target_arch = "riscv32",
    target_arch = "riscv64",

    // exclude apple arm devices
    all(target_arch = "aarch64", not(target_os = "macos"))
))]
pub fn content_process_sandbox_profile() {
    log::error!("Sandboxed multiprocess is not supported on this platform.");
    process::exit(1);
}

/// Returns the executable path to use when spawning a child copy of this binary.
///
/// On Linux, `/proc/self/exe` stays valid even if the on-disk executable is
/// unlinked or replaced while running, whereas `env::current_exe()` can return
/// a `... (deleted)` path that fails with `ENOENT`.
#[cfg_attr(target_os = "ios", allow(dead_code))]
fn current_executable_for_spawn() -> std::io::Result<std::path::PathBuf> {
    #[cfg(target_os = "linux")]
    {
        if std::path::Path::new("/proc/self/exe").exists() {
            return Ok(std::path::PathBuf::from("/proc/self/exe"));
        }
    }
    env::current_exe()
}

#[cfg(any(
    target_os = "windows",
    target_os = "android",
    target_env = "ohos",
    target_arch = "arm",
    target_arch = "aarch64",
    target_arch = "riscv32",
    target_arch = "riscv64"
))]
pub fn spawn_multiprocess(
    content: UnprivilegedContent,
) -> Result<crate::process_manager::Process, ipc_channel::IpcError> {
    use ipc_channel::ipc::{IpcOneShotServer, IpcSender};

    let (server, token) = IpcOneShotServer::<IpcSender<UnprivilegedContent>>::new()
        .map_err(ipc_channel::IpcError::Io)?;

    let path_to_self = current_executable_for_spawn().map_err(ipc_channel::IpcError::Io)?;
    let mut child_process = process::Command::new(path_to_self);
    setup_common(&mut child_process, token);

    #[allow(clippy::zombie_processes)]
    let child = child_process.spawn().map_err(ipc_channel::IpcError::Io)?;

    let (_receiver, sender) = server.accept()?;
    sender.send(content)?;

    Ok(crate::process_manager::Process::Unsandboxed(child))
}

#[cfg(all(
    not(target_os = "windows"),
    not(target_os = "ios"),
    not(target_os = "android"),
    not(target_env = "ohos"),
    not(target_arch = "arm"),
    not(target_arch = "aarch64"),
    not(target_arch = "riscv32"),
    not(target_arch = "riscv64")
))]
pub fn spawn_multiprocess(
    content: UnprivilegedContent,
) -> Result<crate::process_manager::Process, ipc_channel::IpcError> {
    use gaol::sandbox::{self, Sandbox, SandboxMethods};
    use ipc_channel::ipc::{IpcOneShotServer, IpcSender};

    // TODO: Move this impl out of the function. It is only currently here to avoid
    // duplicating the feature flagging.
    #[allow(non_local_definitions)]
    impl CommandMethods for gaol::sandbox::Command {
        fn arg<T>(&mut self, arg: T)
        where
            T: AsRef<OsStr>,
        {
            self.arg(arg);
        }

        fn env<T, U>(&mut self, key: T, val: U)
        where
            T: AsRef<OsStr>,
            U: AsRef<OsStr>,
        {
            self.env(key, val);
        }
    }

    let (server, token) = IpcOneShotServer::<IpcSender<UnprivilegedContent>>::new()
        .map_err(ipc_channel::IpcError::Io)?;

    // If there is a sandbox, use the `gaol` API to create the child process.
    let process = if content.opts().sandbox {
        let mut command = match current_executable_for_spawn() {
            Ok(path_to_self) => sandbox::Command::new(path_to_self),
            Err(_) => sandbox::Command::me().map_err(ipc_channel::IpcError::Io)?,
        };
        setup_common(&mut command, token);

        let profile = content_process_sandbox_profile();
        crate::process_manager::Process::Sandboxed(
            Sandbox::new(profile)
                .start(&mut command)
                .map_err(ipc_channel::IpcError::Io)?
                .pid as u32,
        )
    } else {
        let path_to_self = current_executable_for_spawn().map_err(ipc_channel::IpcError::Io)?;
        let mut child_process = process::Command::new(path_to_self);
        setup_common(&mut child_process, token);

        crate::process_manager::Process::Unsandboxed(
            child_process.spawn().map_err(ipc_channel::IpcError::Io)?,
        )
    };

    let (_receiver, sender) = server.accept()?;
    sender.send(content)?;

    Ok(process)
}

#[cfg(target_os = "ios")]
pub fn spawn_multiprocess(_content: UnprivilegedContent) -> Result<Process, Error> {
    log::error!("Multiprocess is not supported on iOS.");
    process::exit(1);
}

fn setup_common<C: CommandMethods>(command: &mut C, token: String) {
    C::arg(command, "--content-process");
    C::arg(command, token);
    C::env(command, "ROVELO_PARENT_PID", process::id().to_string());

    if let Ok(value) = env::var("RUST_BACKTRACE") {
        C::env(command, "RUST_BACKTRACE", value);
    }

    if let Ok(value) = env::var("RUST_LOG") {
        C::env(command, "RUST_LOG", value);
    }
}

/// A trait to unify commands launched as multiprocess with or without a sandbox.
trait CommandMethods {
    /// A command line argument.
    fn arg<T>(&mut self, arg: T)
    where
        T: AsRef<OsStr>;

    /// An environment variable.
    fn env<T, U>(&mut self, key: T, val: U)
    where
        T: AsRef<OsStr>,
        U: AsRef<OsStr>;
}

impl CommandMethods for process::Command {
    fn arg<T>(&mut self, arg: T)
    where
        T: AsRef<OsStr>,
    {
        self.arg(arg);
    }

    fn env<T, U>(&mut self, key: T, val: U)
    where
        T: AsRef<OsStr>,
        U: AsRef<OsStr>,
    {
        self.env(key, val);
    }
}

#[cfg(test)]
mod tests {
    use super::current_executable_for_spawn;

    #[test]
    #[cfg(target_os = "linux")]
    fn current_executable_for_spawn_returns_proc_self_exe_on_linux() {
        let exe = current_executable_for_spawn()
            .expect("current_executable_for_spawn should succeed on Linux");
        assert_eq!(exe, std::path::PathBuf::from("/proc/self/exe"));
    }
}
