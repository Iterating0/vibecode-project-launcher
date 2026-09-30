//! Native Windows process ownership for the launcher.
//!
//! A process is created suspended, assigned to a private Job Object, and only
//! then resumed.  The Job Object is the ownership boundary used by
//! [`NativeProcess::terminate`]; closing it also terminates every descendant.
//! Standard output and error use anonymous pipes whose read ends are owned by
//! the launcher.  `STARTUPINFOEXW` restricts inheritance to the three child
//! standard handles so a child cannot accidentally inherit handles belonging to
//! another project or to the Tauri host.

#![cfg(windows)]

use std::ffi::{c_void, OsStr};
use std::fs::File;
use std::mem::size_of;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{FromRawHandle, RawHandle};
use std::path::Path;
use std::ptr::{null, null_mut};
use std::thread;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::SetHandleInformation;
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, WAIT_FAILED,
    WAIT_OBJECT_0,
};
use windows_sys::Win32::NetworkManagement::IpHelper::{
    GetExtendedTcpTable, MIB_TCP6ROW_OWNER_PID, MIB_TCPROW_OWNER_PID, TCP_TABLE_OWNER_PID_LISTENER,
};
use windows_sys::Win32::Networking::WinSock::{AF_INET, AF_INET6};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, IsProcessInJob,
    JobObjectBasicAccountingInformation, JobObjectExtendedLimitInformation,
    QueryInformationJobObject, SetInformationJobObject, TerminateJobObject,
    JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    CreateProcessW, DeleteProcThreadAttributeList, GetExitCodeProcess,
    InitializeProcThreadAttributeList, OpenProcess, ResumeThread, TerminateProcess,
    UpdateProcThreadAttribute, WaitForSingleObject, CREATE_NO_WINDOW, CREATE_SUSPENDED,
    EXTENDED_STARTUPINFO_PRESENT, PROCESS_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION,
    PROC_THREAD_ATTRIBUTE_HANDLE_LIST, STARTF_USESTDHANDLES, STARTUPINFOEXW,
};

const ERROR_INSUFFICIENT_BUFFER: u32 = 122;
const EXIT_CODE_ON_TERMINATE: u32 = 1;
const JOB_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

/// A close-on-drop wrapper for an owned Win32 handle.
///
/// Win32 handles are process-wide kernel references and can safely be waited
/// on or used by another thread while another operation terminates the job.
/// The wrapper is therefore explicitly `Send` and `Sync`, while ensuring that
/// the raw handle is closed exactly once.
struct HandleGuard(HANDLE);

unsafe impl Send for HandleGuard {}
unsafe impl Sync for HandleGuard {}

impl HandleGuard {
    fn new(handle: HANDLE) -> Self {
        Self(handle)
    }

    fn raw(&self) -> HANDLE {
        self.0
    }

    fn is_valid(&self) -> bool {
        !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE
    }

    fn take(&mut self) -> HANDLE {
        let handle = self.0;
        self.0 = null_mut();
        handle
    }
}

impl Drop for HandleGuard {
    fn drop(&mut self) {
        if self.is_valid() {
            // Closing a process/job/thread/pipe handle is best effort during
            // unwinding and process shutdown.  The OS owns the final cleanup.
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

/// A process plus the Job Object that owns its complete process tree.
///
/// The public pipe fields are intentionally move/take based.  A process can be
/// wrapped in an `Arc` after callers take these fields, and `wait` and
/// `terminate` can then run concurrently without a `Child` mutex.
pub struct NativeProcess {
    pub pid: u32,
    pub stdout: Option<File>,
    pub stderr: Option<File>,
    process: HandleGuard,
    job: HandleGuard,
}

unsafe impl Send for NativeProcess {}
unsafe impl Sync for NativeProcess {}

impl NativeProcess {
    /// Wait for the root process and return its Windows exit code.
    pub fn wait(&self) -> Result<i32, String> {
        let result = unsafe { WaitForSingleObject(self.process.raw(), u32::MAX) };
        if result == WAIT_FAILED {
            return Err(last_error("WaitForSingleObject"));
        }
        if result != WAIT_OBJECT_0 {
            return Err(format!("WaitForSingleObject returned {result}"));
        }

        let mut code = 0u32;
        if unsafe { GetExitCodeProcess(self.process.raw(), &mut code) } == 0 {
            return Err(last_error("GetExitCodeProcess"));
        }
        Ok(code as i32)
    }

    /// Wait for the root process and then for every process in its Job Object.
    ///
    /// Direct executable/script launches may leave a long-lived descendant
    /// after the root exits (for example a batch file using `start`).  There
    /// is intentionally no short total timeout here: direct applications are
    /// allowed to run indefinitely.  The stop path uses `terminate`, which
    /// keeps its bounded drain timeout.
    pub fn wait_for_tree(&self) -> Result<i32, String> {
        let code = self.wait()?;
        loop {
            match job_active_processes(self.job.raw()) {
                Some(0) => return Ok(code),
                Some(_) => thread::sleep(Duration::from_millis(200)),
                None => return Err(last_error("QueryInformationJobObject")),
            }
        }
    }

    /// Terminate the complete process tree through the private Job Object.
    pub fn terminate(&self) -> Result<(), String> {
        if unsafe { TerminateJobObject(self.job.raw(), EXIT_CODE_ON_TERMINATE) } == 0 {
            let terminate_error = unsafe { GetLastError() };
            // A concurrent watcher may have observed the root process exit
            // just before this call.  Treat an already-empty job as an
            // idempotent successful stop, while preserving real API errors.
            if job_active_processes(self.job.raw()) != Some(0) {
                let error = std::io::Error::from_raw_os_error(terminate_error as i32);
                return Err(format!("TerminateJobObject failed: {error}"));
            }
        }
        wait_for_job_empty(self.job.raw())
    }

    /// Return true when an IPv4 or IPv6 listener on `port` belongs to this
    /// process's Job Object.
    pub fn owns_listening_port(&self, port: u16) -> bool {
        if port == 0 {
            return false;
        }

        let mut pids = listener_pids(AF_INET as u32, false, port);
        pids.extend(listener_pids(AF_INET6 as u32, true, port));
        pids.into_iter()
            .any(|pid| process_belongs_to_job(pid, self.job.raw()))
    }
}

struct PreparedLaunch {
    application_name: Option<Vec<u16>>,
    command_line: Vec<u16>,
}

/// Start `command` through `cmd.exe` in `cwd` and return owned stdout/stderr
/// readers.
pub fn spawn(command: &str, cwd: &Path) -> Result<NativeProcess, String> {
    let launch = PreparedLaunch {
        application_name: None,
        command_line: shell_command_line(command)?,
    };
    spawn_prepared(launch, cwd)
}

/// Start an executable with its explicit application path and raw argument
/// string. `.bat` and `.cmd` files are wrapped by `cmd.exe /D /S /C` so they
/// can use `start` while their descendants stay in the Job Object.
pub fn spawn_executable(
    executable_path: &Path,
    arguments: &str,
    cwd: &Path,
) -> Result<NativeProcess, String> {
    if executable_path.as_os_str().is_empty() {
        return Err("executable path is empty".to_owned());
    }
    validate_cwd(cwd)?;
    let executable = if executable_path.is_absolute() {
        executable_path.to_path_buf()
    } else {
        cwd.join(executable_path)
    };
    if !executable.is_file() {
        return Err(format!(
            "executable does not exist: {}",
            executable.display()
        ));
    }

    let is_batch = executable
        .extension()
        .and_then(OsStr::to_str)
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("bat") || extension.eq_ignore_ascii_case("cmd")
        });
    let launch = if is_batch {
        PreparedLaunch {
            application_name: None,
            command_line: batch_command_line(&executable, arguments)?,
        }
    } else {
        let application_name = wide_path(executable.as_os_str(), "executable path")?;
        let command_line = executable_command_line(&application_name, arguments)?;
        PreparedLaunch {
            application_name: Some(application_name),
            command_line,
        }
    };
    spawn_prepared(launch, cwd)
}

fn spawn_prepared(launch: PreparedLaunch, cwd: &Path) -> Result<NativeProcess, String> {
    validate_cwd(cwd)?;
    let cwd_wide = wide_path(cwd.as_os_str(), "working directory")?;
    let PreparedLaunch {
        application_name,
        mut command_line,
    } = launch;

    // The job is created before the process so all process startup failures
    // naturally release the configured KILL_ON_JOB_CLOSE boundary.
    let job = HandleGuard::new(unsafe { CreateJobObjectW(null(), null()) });
    if !job.is_valid() {
        return Err(last_error("CreateJobObjectW"));
    }
    configure_job(job.raw())?;

    let inherit_attributes = inheritable_attributes();
    let (mut child_stdin, mut stdout_read, mut stderr_read, mut child_stdout, mut child_stderr) =
        create_stdio_pipes(&inherit_attributes)?;

    let inherited_handles = [child_stdin.raw(), child_stdout.raw(), child_stderr.raw()];
    let mut attribute_storage = make_attribute_list(&inherited_handles)?;
    let attribute_list = attribute_storage.as_mut_ptr() as *mut c_void;

    let mut startup = STARTUPINFOEXW::default();
    startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    startup.StartupInfo.hStdInput = child_stdin.raw();
    startup.StartupInfo.hStdOutput = child_stdout.raw();
    startup.StartupInfo.hStdError = child_stderr.raw();
    startup.lpAttributeList = attribute_list;

    let mut process_info = PROCESS_INFORMATION::default();
    let creation_flags = CREATE_SUSPENDED | CREATE_NO_WINDOW | EXTENDED_STARTUPINFO_PRESENT;
    let created = unsafe {
        CreateProcessW(
            application_name
                .as_ref()
                .map_or(null(), |application| application.as_ptr()),
            command_line.as_mut_ptr(),
            null(),
            null(),
            1,
            creation_flags,
            null(),
            cwd_wide.as_ptr(),
            &startup.StartupInfo,
            &mut process_info,
        )
    };

    unsafe {
        DeleteProcThreadAttributeList(attribute_list as _);
    }

    if created == 0 {
        return Err(last_error("CreateProcessW"));
    }

    // The process-information handles are now ours.  The thread must be
    // closed on every path; the process handle remains in NativeProcess.
    let process = HandleGuard::new(process_info.hProcess);
    let thread = HandleGuard::new(process_info.hThread);
    if !process.is_valid() || !thread.is_valid() {
        if process.is_valid() {
            unsafe {
                let _ = TerminateProcess(process.raw(), EXIT_CODE_ON_TERMINATE);
                let _ = WaitForSingleObject(process.raw(), u32::MAX);
            }
        }
        return Err("CreateProcessW returned invalid process handles".to_owned());
    }

    // The child has inherited/duplicated only the handles in the explicit
    // attribute list.  Close all child-side copies in the launcher before
    // returning the parent-side readers.
    close_raw_handle(child_stdin.take());
    close_raw_handle(child_stdout.take());
    close_raw_handle(child_stderr.take());

    if unsafe { AssignProcessToJobObject(job.raw(), process.raw()) } == 0 {
        let error = last_error("AssignProcessToJobObject");
        terminate_unassigned_process(process.raw());
        return Err(error);
    }

    // Resume the primary *thread*.  Passing the process handle here is the
    // subtle bug this module is intended to avoid.
    let resume_result = unsafe { ResumeThread(thread.raw()) };
    if resume_result == u32::MAX {
        let error = last_error("ResumeThread");
        let _ = unsafe { TerminateJobObject(job.raw(), EXIT_CODE_ON_TERMINATE) };
        let _ = unsafe { WaitForSingleObject(process.raw(), u32::MAX) };
        return Err(error);
    }

    let stdout = Some(unsafe { File::from_raw_handle(stdout_read.take() as RawHandle) });
    let stderr = Some(unsafe { File::from_raw_handle(stderr_read.take() as RawHandle) });

    Ok(NativeProcess {
        pid: process_info.dwProcessId,
        stdout,
        stderr,
        process,
        job,
    })
}

fn configure_job(job: HANDLE) -> Result<(), String> {
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    let result = unsafe {
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &limits as *const _ as *const c_void,
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    if result == 0 {
        Err(last_error("SetInformationJobObject"))
    } else {
        Ok(())
    }
}

fn job_active_processes(job: HANDLE) -> Option<u32> {
    let mut accounting = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
    let result = unsafe {
        QueryInformationJobObject(
            job,
            JobObjectBasicAccountingInformation,
            &mut accounting as *mut _ as *mut c_void,
            size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
            null_mut(),
        )
    };
    (result != 0).then_some(accounting.ActiveProcesses)
}

fn wait_for_job_empty(job: HANDLE) -> Result<(), String> {
    let deadline = Instant::now() + JOB_DRAIN_TIMEOUT;
    loop {
        match job_active_processes(job) {
            Some(0) => return Ok(()),
            Some(_) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(10));
            }
            Some(active) => {
                return Err(format!(
                    "TerminateJobObject timed out with {active} active process(es)"
                ));
            }
            None => return Err(last_error("QueryInformationJobObject")),
        }
    }
}

fn create_stdio_pipes(
    attributes: &SECURITY_ATTRIBUTES,
) -> Result<
    (
        HandleGuard,
        HandleGuard,
        HandleGuard,
        HandleGuard,
        HandleGuard,
    ),
    String,
> {
    let mut stdin_read = null_mut();
    let mut stdin_write = null_mut();
    if unsafe { CreatePipe(&mut stdin_read, &mut stdin_write, attributes, 0) } == 0 {
        return Err(last_error("CreatePipe(stdin)"));
    }

    // The launcher never writes to stdin.  Closing this side after process
    // creation makes the child's stdin reach EOF immediately.
    let _stdin_write = HandleGuard::new(stdin_write);
    let stdin = HandleGuard::new(stdin_read);

    let (stdout_read, stdout_write) = create_pipe("stdout", attributes)?;
    let (stderr_read, stderr_write) = create_pipe("stderr", attributes)?;

    Ok((stdin, stdout_read, stderr_read, stdout_write, stderr_write))
}

fn create_pipe(
    name: &str,
    attributes: &SECURITY_ATTRIBUTES,
) -> Result<(HandleGuard, HandleGuard), String> {
    let mut read = null_mut();
    let mut write = null_mut();
    if unsafe { CreatePipe(&mut read, &mut write, attributes, 0) } == 0 {
        return Err(last_error(&format!("CreatePipe({name})")));
    }

    // Only the write end is sent to the child.  Removing inheritance from the
    // launcher-side read end protects against leaked pipe handles.
    if unsafe { SetHandleInformation(read, HANDLE_FLAG_INHERIT, 0) } == 0 {
        unsafe {
            let _ = CloseHandle(read);
            let _ = CloseHandle(write);
        }
        return Err(last_error("SetHandleInformation"));
    }
    Ok((HandleGuard::new(read), HandleGuard::new(write)))
}

/// Build inheritable SECURITY_ATTRIBUTES for the temporary child handles.
fn inheritable_attributes() -> SECURITY_ATTRIBUTES {
    SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: null_mut(),
        bInheritHandle: 1,
    }
}

fn make_attribute_list(handles: &[HANDLE]) -> Result<Vec<usize>, String> {
    if handles.is_empty() || handles.iter().any(|handle| handle.is_null()) {
        return Err("invalid child handle list".to_owned());
    }

    let mut bytes = 0usize;
    let _ = unsafe { InitializeProcThreadAttributeList(null_mut(), 1, 0, &mut bytes) };
    if bytes == 0 {
        return Err(last_error("InitializeProcThreadAttributeList(size)"));
    }

    // Vec<usize> supplies the alignment required by the opaque attribute-list
    // structure.  It remains alive until CreateProcessW has consumed it.
    let words = bytes.div_ceil(size_of::<usize>());
    let mut storage = vec![0usize; words];
    let list = storage.as_mut_ptr() as *mut c_void;
    let mut actual_bytes = bytes;
    if unsafe { InitializeProcThreadAttributeList(list as _, 1, 0, &mut actual_bytes) } == 0 {
        return Err(last_error("InitializeProcThreadAttributeList"));
    }

    let update = unsafe {
        UpdateProcThreadAttribute(
            list as _,
            0,
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
            handles.as_ptr() as *const c_void,
            std::mem::size_of_val(handles),
            null_mut(),
            null(),
        )
    };
    if update == 0 {
        unsafe {
            DeleteProcThreadAttributeList(list as _);
        }
        return Err(last_error("UpdateProcThreadAttribute"));
    }
    Ok(storage)
}

fn wide_path(value: &std::ffi::OsStr, label: &str) -> Result<Vec<u16>, String> {
    if value.encode_wide().any(|unit| unit == 0) {
        return Err(format!("{label} contains an embedded NUL"));
    }
    Ok(value.encode_wide().chain(std::iter::once(0)).collect())
}

fn validate_cwd(cwd: &Path) -> Result<(), String> {
    if cwd.as_os_str().is_empty() {
        return Err("working directory is empty".to_owned());
    }
    if !cwd.is_dir() {
        return Err(format!(
            "working directory does not exist: {}",
            cwd.display()
        ));
    }
    Ok(())
}

fn shell_command_line(command: &str) -> Result<Vec<u16>, String> {
    if command.trim().is_empty() {
        return Err("start command is empty".to_owned());
    }
    if command.encode_utf16().any(|unit| unit == 0) {
        return Err("start command contains an embedded NUL".to_owned());
    }

    // `/S /C` uses the first and last quote as the command boundary.  Keeping
    // the user command intact between them preserves quoted executable paths
    // and arguments such as: node "C:\\Projects\\my app\\server.cjs".
    let mut value = String::with_capacity(command.len() + 24);
    value.push_str("cmd.exe /D /S /C \"");
    value.push_str(command);
    value.push('"');
    Ok(value.encode_utf16().chain(std::iter::once(0)).collect())
}

fn raw_argument_units(arguments: &str) -> Result<Vec<u16>, String> {
    if arguments.encode_utf16().any(|unit| unit == 0) {
        return Err("executable arguments contain an embedded NUL".to_owned());
    }
    Ok(arguments.encode_utf16().collect())
}

fn executable_command_line(application_name: &[u16], arguments: &str) -> Result<Vec<u16>, String> {
    let arguments = raw_argument_units(arguments)?;
    let mut value = Vec::with_capacity(application_name.len() + arguments.len() + 3);
    value.push('"' as u16);
    value.extend(
        application_name
            .iter()
            .copied()
            .take_while(|unit| *unit != 0),
    );
    value.push('"' as u16);
    if !arguments.is_empty() {
        value.push(' ' as u16);
        value.extend(arguments);
    }
    value.push(0);
    Ok(value)
}

fn batch_command_line(executable: &Path, arguments: &str) -> Result<Vec<u16>, String> {
    let path = wide_path(executable.as_os_str(), "batch path")?;
    let arguments = raw_argument_units(arguments)?;
    // `/S /C` consumes the first and last quote as the command boundary.  The
    // inner quote keeps the script path intact when it contains spaces; the
    // final quote is required even when `arguments` is empty.
    let mut value = "cmd.exe /D /S /C \"\"".encode_utf16().collect::<Vec<_>>();
    value.extend(path.iter().copied().take_while(|unit| *unit != 0));
    value.extend("\"".encode_utf16());
    if !arguments.is_empty() {
        value.push(' ' as u16);
        value.extend(arguments);
    }
    value.push('"' as u16);
    value.push(0);
    Ok(value)
}

fn terminate_unassigned_process(process: HANDLE) {
    unsafe {
        let _ = TerminateProcess(process, EXIT_CODE_ON_TERMINATE);
        let _ = WaitForSingleObject(process, u32::MAX);
    }
}

fn close_raw_handle(handle: HANDLE) {
    if !handle.is_null() && handle != INVALID_HANDLE_VALUE {
        unsafe {
            let _ = CloseHandle(handle);
        }
    }
}

fn last_error(operation: &str) -> String {
    let error = std::io::Error::from_raw_os_error(unsafe { GetLastError() } as i32);
    format!("{operation} failed: {error}")
}

fn process_belongs_to_job(pid: u32, job: HANDLE) -> bool {
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() || process == INVALID_HANDLE_VALUE {
        return false;
    }

    let mut in_job = 0;
    let belongs = unsafe { IsProcessInJob(process, job, &mut in_job) } != 0 && in_job != 0;
    unsafe {
        let _ = CloseHandle(process);
    }
    belongs
}

fn listener_pids(address_family: u32, ipv6: bool, port: u16) -> Vec<u32> {
    let row_size = if ipv6 {
        size_of::<MIB_TCP6ROW_OWNER_PID>()
    } else {
        size_of::<MIB_TCPROW_OWNER_PID>()
    };
    let mut size = 0u32;

    // The table can change while it is queried.  Retry a few times if the
    // system reports that the allocation grew between calls.
    for _ in 0..3 {
        let first = unsafe {
            GetExtendedTcpTable(
                null_mut(),
                &mut size,
                0,
                address_family,
                TCP_TABLE_OWNER_PID_LISTENER,
                0,
            )
        };
        if first != ERROR_INSUFFICIENT_BUFFER || size < size_of::<u32>() as u32 {
            return Vec::new();
        }

        let mut bytes = vec![0u8; size as usize];
        let mut actual_size = size;
        let result = unsafe {
            GetExtendedTcpTable(
                bytes.as_mut_ptr() as *mut c_void,
                &mut actual_size,
                0,
                address_family,
                TCP_TABLE_OWNER_PID_LISTENER,
                0,
            )
        };
        if result == 0 {
            let count = unsafe { std::ptr::read_unaligned(bytes.as_ptr() as *const u32) } as usize;
            let available = bytes.len().saturating_sub(size_of::<u32>()) / row_size;
            let mut pids = Vec::new();
            for index in 0..count.min(available) {
                let offset = size_of::<u32>() + index * row_size;
                let row = unsafe { bytes.as_ptr().add(offset) };
                let (local_port, pid) = if ipv6 {
                    let row =
                        unsafe { std::ptr::read_unaligned(row as *const MIB_TCP6ROW_OWNER_PID) };
                    (row.dwLocalPort, row.dwOwningPid)
                } else {
                    let row =
                        unsafe { std::ptr::read_unaligned(row as *const MIB_TCPROW_OWNER_PID) };
                    (row.dwLocalPort, row.dwOwningPid)
                };
                if u16::from_be((local_port & 0xffff) as u16) == port {
                    pids.push(pid);
                }
            }
            return pids;
        }
        if result != ERROR_INSUFFICIENT_BUFFER {
            return Vec::new();
        }
        size = actual_size;
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::process::Command;
    use std::thread;
    use std::time::{Duration, Instant};
    use tempfile::tempdir;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
    };

    #[test]
    fn spawn_echo_captures_stdout_and_waits_without_child_mutex() {
        let directory = tempdir().expect("temporary cwd");
        let mut process = spawn("echo launcher-native-test", directory.path()).expect("spawn");
        let mut stdout = process.stdout.take().expect("stdout pipe");
        let mut output = String::new();
        stdout.read_to_string(&mut output).expect("read stdout");
        assert_eq!(process.wait().expect("wait"), 0);
        assert!(output.contains("launcher-native-test"));
    }

    #[test]
    fn quoted_fixture_command_can_be_stopped_with_its_nested_node_child() {
        let node = node_executable();

        let parent = tempdir().expect("temporary parent");
        let cwd = parent.path().join("cwd with spaces");
        std::fs::create_dir_all(&cwd).expect("cwd");
        let child_pid_path = cwd.join("child pid with spaces.txt");
        let fixture = fixture_path();

        let command = format!(
            "{} \"{}\" --child --port 0 --child-pid-file \"{}\"",
            quote_for_cmd(&node),
            fixture.display(),
            child_pid_path.display()
        );
        let process = spawn(&command, &cwd).expect("spawn fixture");
        let child_pid = wait_for_pid_file(&child_pid_path).expect("nested child pid");

        process.terminate().expect("terminate job");
        let _ = process.wait();
        assert!(wait_for_process_exit(child_pid, Duration::from_secs(5)));
    }

    #[test]
    fn direct_executable_preserves_unicode_space_paths_and_tree_ownership() {
        let node = node_executable();
        let fixture = fixture_path();
        let parent = tempdir().expect("temporary parent");
        let cwd = parent.path().join("直接程序 中文 cwd with spaces");
        std::fs::create_dir_all(&cwd).expect("cwd");
        let child_pid_path = cwd.join("direct child 中文 pid.txt");
        let arguments = format!(
            "\"{}\" --child --port 0 --child-pid-file \"{}\"",
            fixture.display(),
            child_pid_path.display()
        );

        let process = spawn_executable(&node, &arguments, &cwd).expect("spawn direct node");
        let child_pid = wait_for_pid_file(&child_pid_path).expect("direct nested child pid");
        process
            .terminate()
            .expect("terminate direct executable tree");
        let _ = process.wait_for_tree();
        assert!(wait_for_process_exit(child_pid, Duration::from_secs(5)));
    }

    #[test]
    fn direct_batch_start_child_stays_owned_until_termination() {
        let node = node_executable();
        let fixture = fixture_path();
        let parent = tempdir().expect("temporary parent");
        let cwd = parent.path().join("batch 中文 cwd with spaces");
        std::fs::create_dir_all(&cwd).expect("cwd");
        let child_pid_path = cwd.join("batch child 中文 pid.txt");
        let argument_ready_path = cwd.join("batch argument ready 中文 with spaces.txt");
        let script = cwd.join("start child 中文 script.cmd");
        let script_contents = "@echo off\r\nchcp 65001>nul\r\nif /i not \"%~1\"==\"batch argument with spaces\" exit /b 17\r\necho started>batch-start.marker\r\nstart \"\" /b \"%~2\" \"%~3\" --child --port 0 --child-pid-file \"%~4\" --ready-file \"%~5\"\r\n";
        std::fs::write(&script, script_contents).expect("batch fixture");

        let arguments = format!(
            "\"batch argument with spaces\" \"{}\" \"{}\" \"{}\" \"{}\"",
            node.display(),
            fixture.display(),
            child_pid_path.display(),
            argument_ready_path.display()
        );
        let process = spawn_executable(&script, &arguments, &cwd).expect("spawn batch fixture");
        assert!(
            wait_for_file(&cwd.join("batch-start.marker")),
            "batch script did not execute"
        );
        assert!(
            wait_for_file(&argument_ready_path),
            "batch argument did not reach fixture"
        );
        let child_pid = wait_for_pid_file(&child_pid_path).expect("batch nested child pid");
        process
            .terminate()
            .expect("terminate batch descendant tree");
        process
            .wait_for_tree()
            .expect("batch root and descendants stopped");
        assert!(wait_for_process_exit(child_pid, Duration::from_secs(5)));
    }

    #[test]
    fn ipv6_listener_is_owned_by_the_job() {
        let node = node_executable();
        let parent = tempdir().expect("temporary parent");
        let cwd = parent.path().join("ipv6 cwd with spaces");
        std::fs::create_dir_all(&cwd).expect("cwd");
        let ready_file = cwd.join("ipv6 ready with spaces.txt");
        let fixture = fixture_path();
        let command = format!(
            "{} \"{}\" --host ::1 --port 0 --ready-file \"{}\"",
            quote_for_cmd(&node),
            fixture.display(),
            ready_file.display()
        );

        let process = spawn(&command, &cwd).expect("spawn IPv6 fixture");
        let port = wait_for_port_file(&ready_file).expect("IPv6 fixture port");
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut owned = false;
        while Instant::now() < deadline {
            if process.owns_listening_port(port) {
                owned = true;
                break;
            }
            thread::sleep(Duration::from_millis(25));
        }
        assert!(
            owned,
            "IPv6 listener on {port} was not attributed to the job"
        );
        process.terminate().expect("terminate IPv6 fixture");
        process.wait().expect("wait IPv6 fixture");
    }

    fn node_executable() -> std::path::PathBuf {
        let output = Command::new("where.exe")
            .arg("node")
            .output()
            .expect("where.exe is required for native process tests");
        assert!(output.status.success(), "Node.js --version failed");
        let node = String::from_utf8_lossy(&output.stdout)
            .lines()
            .next()
            .map(str::trim)
            .filter(|path| !path.is_empty())
            .map(std::path::PathBuf::from)
            .expect("where.exe node result");
        assert!(
            node.is_file(),
            "Node executable missing: {}",
            node.display()
        );
        node
    }

    fn fixture_path() -> std::path::PathBuf {
        let fixture = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("tests")
            .join("fixtures")
            .join("fixture-server.cjs");
        assert!(fixture.is_file(), "fixture missing: {}", fixture.display());
        fixture
    }

    fn quote_for_cmd(value: &Path) -> String {
        format!("\"{}\"", value.display())
    }

    fn wait_for_pid_file(path: &Path) -> Option<u32> {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if let Ok(value) = std::fs::read_to_string(path) {
                if let Ok(pid) = value.trim().parse() {
                    return Some(pid);
                }
            }
            thread::sleep(Duration::from_millis(50));
        }
        None
    }

    fn wait_for_file(path: &Path) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if path.is_file() {
                return true;
            }
            thread::sleep(Duration::from_millis(50));
        }
        false
    }

    fn wait_for_port_file(path: &Path) -> Option<u16> {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if let Ok(value) = std::fs::read_to_string(path) {
                if let Ok(port) = value.trim().parse() {
                    return Some(port);
                }
            }
            thread::sleep(Duration::from_millis(50));
        }
        None
    }

    fn wait_for_process_exit(pid: u32, timeout: Duration) -> bool {
        let process = unsafe {
            OpenProcess(
                PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
                0,
                pid,
            )
        };
        if process.is_null() || process == INVALID_HANDLE_VALUE {
            return true;
        }
        let result = unsafe {
            WaitForSingleObject(process, timeout.as_millis().min(u32::MAX as u128) as u32)
        };
        unsafe {
            let _ = CloseHandle(process);
        }
        result == WAIT_OBJECT_0
    }
}
