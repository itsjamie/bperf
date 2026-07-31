//! Browser process ownership and NUL-framed protocol pipes.
//!
//! Chromium's remote-debugging pipe, Firefox's Juggler pipe, and Playwright
//! WebKit's inspector pipe use child descriptor 3 for commands and descriptor 4
//! for responses. Windows receives those descriptors through the CRT table in
//! `STARTUPINFO.lpReserved2`; Unix receives them with `dup2`.

use std::{
    fs::File,
    io::{Read, Write},
    path::Path,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use serde_json::Value;
use tempfile::TempDir;

const GRACEFUL_EXIT_TIMEOUT: Duration = Duration::from_secs(5);
const FORCED_EXIT_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) struct BrowserProcess {
    child: platform::ChildProcess,
    writer: File,
    frames: mpsc::Receiver<Result<Vec<u8>, String>>,
    _working_directory: TempDir,
    contained_processes_stopped: bool,
}

impl BrowserProcess {
    pub(crate) fn spawn(
        temporary_prefix: &str,
        log_label: &'static str,
        executable: &Path,
        arguments: &[String],
    ) -> Result<Self> {
        Self::spawn_configured(temporary_prefix, log_label, executable, &[], |_| {
            Ok(arguments.to_vec())
        })
    }

    pub(crate) fn spawn_configured(
        temporary_prefix: &str,
        log_label: &'static str,
        executable: &Path,
        environment_removals: &[&str],
        arguments: impl FnOnce(&Path) -> Result<Vec<String>>,
    ) -> Result<Self> {
        let working_directory = tempfile::Builder::new()
            .prefix(temporary_prefix)
            .tempdir()
            .context("failed to create an isolated browser process directory")?;
        let arguments = arguments(working_directory.path())?;
        let spawned = platform::spawn(
            executable,
            &arguments,
            working_directory.path(),
            environment_removals,
        )?;
        for (label, reader) in spawned.logs {
            thread::spawn(move || {
                let mut lines = std::io::BufReader::new(reader);
                let mut line = String::new();
                loop {
                    line.clear();
                    match std::io::BufRead::read_line(&mut lines, &mut line) {
                        Ok(0) => break,
                        Ok(_) => eprint!("[{log_label}:{label}] {line}"),
                        Err(_) => break,
                    }
                }
            });
        }

        let (sender, frames) = mpsc::sync_channel(32);
        thread::spawn(move || {
            let mut reader = spawned.reader;
            let mut decoder = NulFrameDecoder::default();
            let mut bytes = [0_u8; 64 * 1024];
            loop {
                match reader.read(&mut bytes) {
                    Ok(0) => {
                        if decoder.has_partial_frame() {
                            let _ =
                                sender
                                    .send(Err("browser protocol pipe closed inside a JSON frame"
                                        .to_owned()));
                        }
                        break;
                    }
                    Ok(count) => {
                        for frame in decoder.push(&bytes[..count]) {
                            if sender.send(Ok(frame)).is_err() {
                                return;
                            }
                        }
                    }
                    Err(error) => {
                        let _ = sender.send(Err(format!(
                            "failed reading browser protocol pipe: {error}"
                        )));
                        break;
                    }
                }
            }
        });

        Ok(Self {
            child: spawned.child,
            writer: spawned.writer,
            frames,
            _working_directory: working_directory,
            contained_processes_stopped: false,
        })
    }

    pub(crate) fn working_directory(&self) -> &Path {
        self._working_directory.path()
    }

    pub(crate) fn pid(&self) -> u32 {
        self.child.pid()
    }

    pub(crate) fn send(&mut self, message: &Value) -> Result<()> {
        serde_json::to_writer(&mut self.writer, message)
            .context("failed to encode a browser protocol message")?;
        self.writer
            .write_all(&[0])
            .context("failed to terminate a browser protocol message")?;
        self.writer
            .flush()
            .context("failed to flush a browser protocol message")
    }

    pub(crate) fn receive(&self, timeout: Duration) -> Result<Value> {
        let frame = match self.frames.recv_timeout(timeout) {
            Ok(Ok(frame)) => frame,
            Ok(Err(message)) => bail!("{message}"),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                bail!("browser protocol pipe did not respond within {timeout:?}")
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                bail!("browser protocol pipe closed before responding")
            }
        };
        serde_json::from_slice(&frame).context("browser protocol pipe emitted invalid JSON")
    }

    pub(crate) fn wait_for_exit(&mut self) -> Result<()> {
        let deadline = Instant::now() + GRACEFUL_EXIT_TIMEOUT;
        loop {
            if self.child.root_has_exited()? {
                self.stop_contained_processes()?;
                return Ok(());
            }
            if Instant::now() >= deadline {
                // Profiles are isolated and disposable; cleanup completeness
                // matters, not whether background browser work allowed a
                // voluntary exit.
                return self.stop_contained_processes();
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    pub(crate) fn terminate(&mut self) -> Result<()> {
        self.stop_contained_processes()
    }

    fn stop_contained_processes(&mut self) -> Result<()> {
        self.child.stop_contained_processes()?;
        self.contained_processes_stopped = true;
        Ok(())
    }
}

impl Drop for BrowserProcess {
    fn drop(&mut self) {
        if !self.contained_processes_stopped {
            // Terminate before reaping the root so its process-group id cannot
            // be reused between the two operations.
            let _ = self.stop_contained_processes();
        }
    }
}

#[derive(Default)]
struct NulFrameDecoder {
    pending: Vec<u8>,
}

impl NulFrameDecoder {
    fn push(&mut self, bytes: &[u8]) -> Vec<Vec<u8>> {
        let mut frames = Vec::new();
        let mut start = 0;
        for (index, byte) in bytes.iter().enumerate() {
            if *byte != 0 {
                continue;
            }
            self.pending.extend_from_slice(&bytes[start..index]);
            frames.push(std::mem::take(&mut self.pending));
            start = index + 1;
        }
        self.pending.extend_from_slice(&bytes[start..]);
        frames
    }

    fn has_partial_frame(&self) -> bool {
        !self.pending.is_empty()
    }
}

struct SpawnedProcess {
    child: platform::ChildProcess,
    writer: File,
    reader: File,
    logs: Vec<(&'static str, File)>,
}

#[cfg(unix)]
mod platform {
    use std::{
        fs::File,
        os::{
            fd::{FromRawFd, OwnedFd, RawFd},
            unix::process::CommandExt,
        },
        path::Path,
        process::{Child, Command, Stdio},
        thread,
        time::{Duration, Instant},
    };

    use anyhow::{Context, Result, bail};

    use super::{FORCED_EXIT_TIMEOUT, SpawnedProcess};

    pub(super) struct ChildProcess {
        child: Child,
        process_group: i32,
        process_group_terminated: bool,
    }

    impl ChildProcess {
        pub(super) fn pid(&self) -> u32 {
            self.child.id()
        }

        pub(super) fn root_has_exited(&self) -> Result<bool> {
            let mut information = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
            let result = unsafe {
                libc::waitid(
                    libc::P_PID,
                    self.process_group as libc::id_t,
                    information.as_mut_ptr(),
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            };
            if result == -1 {
                return Err(std::io::Error::last_os_error())
                    .context("failed to observe browser exit");
            }
            let information = unsafe { information.assume_init() };
            Ok(unsafe { information.si_pid() } != 0)
        }

        pub(super) fn stop_contained_processes(&mut self) -> Result<()> {
            if !self.process_group_terminated {
                self.terminate_process_group()?;
                self.process_group_terminated = true;
            }
            self.child.wait()?;
            self.wait_until_empty()
        }

        fn terminate_process_group(&self) -> Result<()> {
            // A gracefully exited root remains waitable until this signal has
            // been sent, so its process-group id cannot identify another group.
            let result = unsafe { libc::kill(-self.process_group, libc::SIGKILL) };
            if result == -1 {
                let error = std::io::Error::last_os_error();
                match error.raw_os_error() {
                    Some(libc::ESRCH) => {}
                    Some(libc::EPERM) if cfg!(target_os = "macos") && self.root_has_exited()? => {
                        // XNU excludes zombies from process-group signaling and
                        // reports EPERM when no live member can be signaled.
                        // Reaping the retained leader before the emptiness check
                        // distinguishes that state from inaccessible live members.
                    }
                    _ => {
                        return Err(error).context("failed to terminate the browser process group");
                    }
                }
            }
            Ok(())
        }

        fn wait_until_empty(&self) -> Result<()> {
            let deadline = Instant::now() + FORCED_EXIT_TIMEOUT;
            loop {
                let result = unsafe { libc::kill(-self.process_group, 0) };
                if result == -1 {
                    let error = std::io::Error::last_os_error();
                    match error.raw_os_error() {
                        Some(libc::ESRCH) => return Ok(()),
                        Some(libc::EPERM) => {}
                        _ => {
                            return Err(error)
                                .context("failed to inspect the browser process group");
                        }
                    }
                }
                if Instant::now() >= deadline {
                    bail!(
                        "browser process group {} still contains processes after termination",
                        self.process_group
                    );
                }
                thread::sleep(Duration::from_millis(10));
            }
        }
    }

    pub(super) fn spawn(
        executable: &Path,
        arguments: &[String],
        working_directory: &Path,
        environment_removals: &[&str],
    ) -> Result<SpawnedProcess> {
        let (child_read, parent_write) = pipe()?;
        let (parent_read, child_write) = pipe()?;
        let child_read = duplicate_high(child_read)?;
        let child_write = duplicate_high(child_write)?;

        let mut command = Command::new(executable);
        command
            .args(arguments)
            .current_dir(working_directory)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        for variable in environment_removals {
            command.env_remove(variable);
        }
        unsafe {
            command.pre_exec(move || {
                if libc::dup2(child_read, 3) == -1 || libc::dup2(child_write, 4) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                libc::close(child_read);
                libc::close(child_write);
                Ok(())
            });
        }
        let spawn = command.spawn();
        unsafe {
            libc::close(child_read);
            libc::close(child_write);
        }
        let mut child = spawn.with_context(|| {
            format!(
                "failed to launch browser executable {}",
                executable.display()
            )
        })?;
        let pid = i32::try_from(child.id()).context("browser PID does not fit a process group")?;
        let stdout = child
            .stdout
            .take()
            .context("browser stdout was unavailable")?;
        let stderr = child
            .stderr
            .take()
            .context("browser stderr was unavailable")?;

        Ok(SpawnedProcess {
            child: ChildProcess {
                child,
                process_group: pid,
                process_group_terminated: false,
            },
            writer: unsafe { File::from_raw_fd(parent_write) },
            reader: unsafe { File::from_raw_fd(parent_read) },
            logs: vec![
                ("stdout", File::from(OwnedFd::from(stdout))),
                ("stderr", File::from(OwnedFd::from(stderr))),
            ],
        })
    }

    fn pipe() -> Result<(RawFd, RawFd)> {
        let mut descriptors = [-1; 2];
        if unsafe { libc::pipe(descriptors.as_mut_ptr()) } == -1 {
            return Err(std::io::Error::last_os_error())
                .context("failed to create a browser protocol pipe");
        }
        for descriptor in descriptors {
            let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFD) };
            if flags == -1
                || unsafe { libc::fcntl(descriptor, libc::F_SETFD, flags | libc::FD_CLOEXEC) } == -1
            {
                unsafe {
                    libc::close(descriptors[0]);
                    libc::close(descriptors[1]);
                }
                bail!(
                    "failed to make a browser protocol descriptor close-on-exec: {}",
                    std::io::Error::last_os_error()
                );
            }
        }
        Ok((descriptors[0], descriptors[1]))
    }

    fn duplicate_high(descriptor: RawFd) -> Result<RawFd> {
        let duplicated = unsafe { libc::fcntl(descriptor, libc::F_DUPFD_CLOEXEC, 10) };
        let error = std::io::Error::last_os_error();
        unsafe {
            libc::close(descriptor);
        }
        if duplicated == -1 {
            return Err(error).context("failed to reserve a browser child descriptor");
        }
        Ok(duplicated)
    }
}

#[cfg(windows)]
mod platform {
    use std::{
        ffi::{OsStr, c_void},
        fs::File,
        mem::{size_of, zeroed},
        os::windows::{
            ffi::OsStrExt,
            io::{AsRawHandle, FromRawHandle, OwnedHandle},
        },
        path::Path,
        ptr::{null, null_mut},
        thread,
        time::{Duration, Instant},
    };

    use anyhow::{Context, Result, bail};
    use windows_sys::Win32::{
        Foundation::{
            HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, STILL_ACTIVE, SetHandleInformation,
            WAIT_OBJECT_0, WAIT_TIMEOUT,
        },
        Security::SECURITY_ATTRIBUTES,
        Storage::FileSystem::{
            CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_GENERIC_READ, FILE_SHARE_READ,
            FILE_SHARE_WRITE, OPEN_EXISTING,
        },
        System::{
            JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW,
                JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
                JobObjectBasicAccountingInformation, JobObjectExtendedLimitInformation,
                QueryInformationJobObject, SetInformationJobObject, TerminateJobObject,
            },
            Pipes::CreatePipe,
            Threading::{
                CREATE_NO_WINDOW, CREATE_SUSPENDED, CreateProcessW, DeleteProcThreadAttributeList,
                EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess,
                InitializeProcThreadAttributeList, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
                PROCESS_INFORMATION, ResumeThread, STARTF_USESTDHANDLES, STARTUPINFOEXW,
                TerminateProcess, UpdateProcThreadAttribute, WaitForSingleObject,
            },
        },
    };

    use super::{FORCED_EXIT_TIMEOUT, SpawnedProcess};

    const FD_COUNT: usize = 5;
    const CRT_FOPEN: u8 = 0x01;
    const CRT_FPIPE: u8 = 0x08;
    const CRT_FDEV: u8 = 0x40;
    const FORCED_EXIT_TIMEOUT_MS: u32 = 5_000;

    pub(super) struct ChildProcess {
        process: OwnedHandle,
        job: OwnedHandle,
        pid: u32,
    }

    impl ChildProcess {
        pub(super) fn pid(&self) -> u32 {
            self.pid
        }

        pub(super) fn root_has_exited(&self) -> Result<bool> {
            let result = unsafe { WaitForSingleObject(self.process_handle(), 0) };
            if result == WAIT_TIMEOUT {
                Ok(false)
            } else if result == WAIT_OBJECT_0 {
                let mut exit_code = 0;
                if unsafe { GetExitCodeProcess(self.process_handle(), &mut exit_code) } == 0 {
                    return Err(std::io::Error::last_os_error())
                        .context("failed to read browser exit status");
                }
                if exit_code == STILL_ACTIVE as u32 {
                    Ok(false)
                } else {
                    Ok(true)
                }
            } else {
                Err(std::io::Error::last_os_error()).context("failed waiting for browser")
            }
        }

        pub(super) fn stop_contained_processes(&mut self) -> Result<()> {
            self.terminate_job()?;
            let result =
                unsafe { WaitForSingleObject(self.process_handle(), FORCED_EXIT_TIMEOUT_MS) };
            if result == WAIT_TIMEOUT {
                bail!("browser process did not exit after termination");
            }
            if result != WAIT_OBJECT_0 {
                return Err(std::io::Error::last_os_error()).context("failed waiting for browser");
            }
            self.wait_until_empty()
        }

        fn terminate_job(&self) -> Result<()> {
            if unsafe { TerminateJobObject(self.job_handle(), 1) } == 0 {
                return Err(std::io::Error::last_os_error())
                    .context("failed to terminate the browser Job Object");
            }
            Ok(())
        }

        fn wait_until_empty(&self) -> Result<()> {
            let deadline = Instant::now() + FORCED_EXIT_TIMEOUT;
            loop {
                let mut information = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
                if unsafe {
                    QueryInformationJobObject(
                        self.job_handle(),
                        JobObjectBasicAccountingInformation,
                        (&mut information as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
                        size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                        null_mut(),
                    )
                } == 0
                {
                    return Err(std::io::Error::last_os_error())
                        .context("failed to inspect the browser Job Object");
                }
                if information.ActiveProcesses == 0 {
                    return Ok(());
                }
                if Instant::now() >= deadline {
                    bail!(
                        "browser Job Object still contains {} process(es) after termination",
                        information.ActiveProcesses
                    );
                }
                thread::sleep(Duration::from_millis(10));
            }
        }

        fn process_handle(&self) -> HANDLE {
            self.process.as_raw_handle() as HANDLE
        }

        fn job_handle(&self) -> HANDLE {
            self.job.as_raw_handle() as HANDLE
        }
    }

    pub(super) fn spawn(
        executable: &Path,
        arguments: &[String],
        working_directory: &Path,
        environment_removals: &[&str],
    ) -> Result<SpawnedProcess> {
        debug_assert!(
            environment_removals.is_empty(),
            "Windows browser environment removal requires an explicit environment block"
        );
        let security = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: null_mut(),
            bInheritHandle: 1,
        };
        let (child_read, parent_write) = anonymous_pipe(&security, true)?;
        let (parent_read, child_write) = anonymous_pipe(&security, false)?;
        let (stdout_read, stdout_write) = anonymous_pipe(&security, false)?;
        let (stderr_read, stderr_write) = anonymous_pipe(&security, false)?;
        let stdin = open_nul(FILE_GENERIC_READ, &security)?;

        let handles = [
            raw_handle(&stdin),
            raw_handle(&stdout_write),
            raw_handle(&stderr_write),
            raw_handle(&child_read),
            raw_handle(&child_write),
        ];
        let mut descriptor_table = crt_descriptor_table(handles);
        let attributes = InheritedHandleList::new(&handles)?;
        let mut startup: STARTUPINFOEXW = unsafe { zeroed() };
        startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
        startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        startup.StartupInfo.cbReserved2 = u16::try_from(descriptor_table.len())
            .context("browser descriptor table is too large")?;
        startup.StartupInfo.lpReserved2 = descriptor_table.as_mut_ptr();
        startup.StartupInfo.hStdInput = handles[0];
        startup.StartupInfo.hStdOutput = handles[1];
        startup.StartupInfo.hStdError = handles[2];
        startup.lpAttributeList = attributes.pointer;

        let executable_wide = wide_null(executable.as_os_str());
        let mut command_line = wide_null(OsStr::new(
            &std::iter::once(executable.to_string_lossy().into_owned())
                .chain(arguments.iter().cloned())
                .map(|argument| quote_windows_argument(&argument))
                .collect::<Vec<_>>()
                .join(" "),
        ));
        let working_directory_wide = wide_null(working_directory.as_os_str());
        let mut process_info: PROCESS_INFORMATION = unsafe { zeroed() };
        let job = create_kill_on_close_job()?;
        let created = unsafe {
            CreateProcessW(
                executable_wide.as_ptr(),
                command_line.as_mut_ptr(),
                null(),
                null(),
                1,
                CREATE_NO_WINDOW | CREATE_SUSPENDED | EXTENDED_STARTUPINFO_PRESENT,
                null(),
                working_directory_wide.as_ptr(),
                &startup.StartupInfo,
                &mut process_info,
            )
        };
        if created == 0 {
            return Err(std::io::Error::last_os_error()).with_context(|| {
                format!(
                    "failed to launch browser executable {}",
                    executable.display()
                )
            });
        }
        let process = unsafe { OwnedHandle::from_raw_handle(process_info.hProcess) };
        let thread = unsafe { OwnedHandle::from_raw_handle(process_info.hThread) };
        if unsafe {
            AssignProcessToJobObject(
                job.as_raw_handle() as HANDLE,
                process.as_raw_handle() as HANDLE,
            )
        } == 0
        {
            let error = std::io::Error::last_os_error();
            if unsafe { TerminateProcess(process.as_raw_handle() as HANDLE, 1) } == 0 {
                let terminate_error = std::io::Error::last_os_error();
                bail!(
                    "failed to assign browser to its kill-on-close Job Object: {error}; the suspended process also could not be terminated: {terminate_error}"
                );
            }
            unsafe {
                WaitForSingleObject(process.as_raw_handle() as HANDLE, FORCED_EXIT_TIMEOUT_MS);
            }
            bail!("failed to assign browser to its kill-on-close Job Object: {error}");
        }
        if unsafe { ResumeThread(thread.as_raw_handle() as HANDLE) } == u32::MAX {
            unsafe {
                TerminateJobObject(job.as_raw_handle() as HANDLE, 1);
            }
            return Err(std::io::Error::last_os_error()).context("failed to resume browser");
        }
        drop(thread);
        drop(child_read);
        drop(child_write);
        drop(stdin);
        drop(stdout_write);
        drop(stderr_write);

        Ok(SpawnedProcess {
            child: ChildProcess {
                process,
                job,
                pid: process_info.dwProcessId,
            },
            writer: File::from(parent_write),
            reader: File::from(parent_read),
            logs: vec![
                ("stdout", File::from(stdout_read)),
                ("stderr", File::from(stderr_read)),
            ],
        })
    }

    struct InheritedHandleList {
        _storage: Vec<usize>,
        pointer: *mut c_void,
    }

    impl InheritedHandleList {
        fn new(handles: &[HANDLE]) -> Result<Self> {
            let mut byte_count = 0;
            unsafe {
                InitializeProcThreadAttributeList(null_mut(), 1, 0, &mut byte_count);
            }
            if byte_count == 0 {
                return Err(std::io::Error::last_os_error())
                    .context("failed to size the browser inherited-handle list");
            }
            let word_count = byte_count.div_ceil(size_of::<usize>());
            let mut storage = vec![0_usize; word_count];
            let pointer = storage.as_mut_ptr().cast::<c_void>();
            if unsafe { InitializeProcThreadAttributeList(pointer, 1, 0, &mut byte_count) } == 0 {
                return Err(std::io::Error::last_os_error())
                    .context("failed to initialize the browser inherited-handle list");
            }
            if unsafe {
                UpdateProcThreadAttribute(
                    pointer,
                    0,
                    PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                    handles.as_ptr().cast::<c_void>(),
                    std::mem::size_of_val(handles),
                    null_mut(),
                    null(),
                )
            } == 0
            {
                unsafe {
                    DeleteProcThreadAttributeList(pointer);
                }
                return Err(std::io::Error::last_os_error())
                    .context("failed to restrict browser inherited handles");
            }
            Ok(Self {
                _storage: storage,
                pointer,
            })
        }
    }

    impl Drop for InheritedHandleList {
        fn drop(&mut self) {
            unsafe {
                DeleteProcThreadAttributeList(self.pointer);
            }
        }
    }

    fn anonymous_pipe(
        security: &SECURITY_ATTRIBUTES,
        parent_writes: bool,
    ) -> Result<(OwnedHandle, OwnedHandle)> {
        let mut read = null_mut();
        let mut write = null_mut();
        if unsafe { CreatePipe(&mut read, &mut write, security, 0) } == 0 {
            return Err(std::io::Error::last_os_error())
                .context("failed to create a browser protocol pipe");
        }
        let read = unsafe { OwnedHandle::from_raw_handle(read) };
        let write = unsafe { OwnedHandle::from_raw_handle(write) };
        let parent = if parent_writes { &write } else { &read };
        if unsafe { SetHandleInformation(parent.as_raw_handle() as HANDLE, HANDLE_FLAG_INHERIT, 0) }
            == 0
        {
            return Err(std::io::Error::last_os_error())
                .context("failed to make a browser parent pipe non-inheritable");
        }
        Ok((read, write))
    }

    fn open_nul(access: u32, security: &SECURITY_ATTRIBUTES) -> Result<OwnedHandle> {
        let name: Vec<u16> = OsStr::new("NUL").encode_wide().chain(Some(0)).collect();
        let handle = unsafe {
            CreateFileW(
                name.as_ptr(),
                access,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                security,
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error()).context("failed to open NUL for browser");
        }
        Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
    }

    fn create_kill_on_close_job() -> Result<OwnedHandle> {
        let handle = unsafe { CreateJobObjectW(null(), null()) };
        if handle.is_null() {
            return Err(std::io::Error::last_os_error())
                .context("failed to create a browser Job Object");
        }
        let job = unsafe { OwnedHandle::from_raw_handle(handle) };
        let mut information: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
        information.BasicLimitInformation.LimitFlags =
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION;
        if unsafe {
            SetInformationJobObject(
                job.as_raw_handle() as HANDLE,
                JobObjectExtendedLimitInformation,
                &information as *const _ as *const c_void,
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error())
                .context("failed to configure the browser Job Object");
        }
        Ok(job)
    }

    pub(super) fn crt_descriptor_table(handles: [HANDLE; FD_COUNT]) -> Vec<u8> {
        let handle_offset = size_of::<u32>() + FD_COUNT;
        let mut table = vec![0_u8; handle_offset + size_of::<HANDLE>() * FD_COUNT];
        table[..size_of::<u32>()].copy_from_slice(&(FD_COUNT as u32).to_ne_bytes());
        for (index, handle) in handles.into_iter().enumerate() {
            table[size_of::<u32>() + index] = if index <= 2 {
                CRT_FOPEN | CRT_FDEV
            } else {
                CRT_FOPEN | CRT_FPIPE
            };
            unsafe {
                (table
                    .as_mut_ptr()
                    .add(handle_offset + size_of::<HANDLE>() * index)
                    as *mut HANDLE)
                    .write_unaligned(handle);
            }
        }
        table
    }

    fn raw_handle(handle: &OwnedHandle) -> HANDLE {
        handle.as_raw_handle() as HANDLE
    }

    fn wide_null(value: &OsStr) -> Vec<u16> {
        value.encode_wide().chain(Some(0)).collect()
    }

    fn quote_windows_argument(argument: &str) -> String {
        if !argument.is_empty()
            && !argument
                .chars()
                .any(|character| character.is_whitespace() || character == '"')
        {
            return argument.to_owned();
        }
        let mut quoted = String::from("\"");
        let mut backslashes = 0;
        for character in argument.chars() {
            if character == '\\' {
                backslashes += 1;
            } else if character == '"' {
                quoted.push_str(&"\\".repeat(backslashes * 2 + 1));
                quoted.push('"');
                backslashes = 0;
            } else {
                quoted.push_str(&"\\".repeat(backslashes));
                backslashes = 0;
                quoted.push(character);
            }
        }
        quoted.push_str(&"\\".repeat(backslashes * 2));
        quoted.push('"');
        quoted
    }
}

#[cfg(test)]
mod tests {
    use super::NulFrameDecoder;

    #[cfg(unix)]
    #[test]
    fn unix_child_receives_protocol_descriptors_three_and_four() {
        use std::{
            io::{Read, Write},
            path::Path,
        };

        let working_directory = tempfile::tempdir().unwrap();
        let mut spawned = super::platform::spawn(
            Path::new("/bin/sh"),
            &[
                "-c".to_owned(),
                "IFS= read -r message <&3; printf '%s\\0' \"$message\" >&4".to_owned(),
            ],
            working_directory.path(),
            &[],
        )
        .unwrap();
        spawned.writer.write_all(b"{\"id\":1}\n").unwrap();
        spawned.writer.flush().unwrap();
        let mut response = Vec::new();
        spawned.reader.read_to_end(&mut response).unwrap();
        spawned.child.stop_contained_processes().unwrap();

        assert_eq!(response, b"{\"id\":1}\0");
    }

    #[cfg(unix)]
    #[test]
    fn unix_exit_observation_preserves_the_process_group_leader_until_termination() {
        use std::{
            path::Path,
            thread,
            time::{Duration, Instant},
        };

        let working_directory = tempfile::tempdir().unwrap();
        let mut spawned = super::platform::spawn(
            Path::new("/bin/sh"),
            &["-c".to_owned(), "exit 0".to_owned()],
            working_directory.path(),
            &[],
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !spawned.child.root_has_exited().unwrap() {
            assert!(Instant::now() < deadline, "shell did not exit");
            thread::sleep(Duration::from_millis(10));
        }

        let mut information = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                spawned.child.pid() as libc::id_t,
                information.as_mut_ptr(),
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        assert_eq!(result, 0);
        assert_eq!(
            unsafe { information.assume_init().si_pid() },
            spawned.child.pid() as libc::pid_t
        );

        spawned.child.stop_contained_processes().unwrap();
        spawned.child.stop_contained_processes().unwrap();
    }

    #[test]
    fn protocol_frames_may_be_fragmented_or_coalesced() {
        let mut decoder = NulFrameDecoder::default();
        assert!(decoder.push(br#"{"id":1"#).is_empty());
        assert_eq!(
            decoder.push(b"}\0{\"id\":2}\0{\"id\""),
            [br#"{"id":1}"#.to_vec(), br#"{"id":2}"#.to_vec()]
        );
        assert_eq!(decoder.push(b":3}\0"), [br#"{"id":3}"#.to_vec()]);
        assert!(!decoder.has_partial_frame());
    }

    #[test]
    fn protocol_framer_preserves_large_heap_payloads() {
        let payload = vec![b'x'; 2 * 1024 * 1024];
        let mut decoder = NulFrameDecoder::default();
        assert!(decoder.push(&payload[..1_000_000]).is_empty());
        let mut tail = payload[1_000_000..].to_vec();
        tail.push(0);
        assert_eq!(decoder.push(&tail), [payload]);
    }

    #[cfg(windows)]
    #[test]
    fn windows_descriptor_table_matches_the_crt_reserved2_layout() {
        use std::{mem::size_of, ptr::read_unaligned};

        use windows_sys::Win32::Foundation::HANDLE;

        let handles = [1, 2, 3, 4, 5].map(|value| value as HANDLE);
        let table = super::platform::crt_descriptor_table(handles);
        assert_eq!(
            u32::from_ne_bytes(table[..4].try_into().unwrap()),
            handles.len() as u32
        );
        assert_eq!(&table[4..9], &[0x41, 0x41, 0x41, 0x09, 0x09]);
        for (index, expected) in handles.into_iter().enumerate() {
            let offset = 4 + handles.len() + index * size_of::<HANDLE>();
            let actual = unsafe { read_unaligned(table.as_ptr().add(offset) as *const HANDLE) };
            assert_eq!(actual, expected);
        }
    }
}
