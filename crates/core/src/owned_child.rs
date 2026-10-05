//! Child processes that are owned together with every process they start.
//!
//! Killing a [`std::process::Child`] only kills that one process. Anything it
//! started keeps running (ex. the commands a shell runs, or on Windows the
//! `node.exe` of a `.cmd` shim, which runs through `cmd.exe`), and a killed
//! child that isn't waited on lingers as a zombie on unix. An [`OwnedChild`]
//! runs the child in a group of its own (a process group on unix, a job object
//! on Windows), kills the whole group when it's killed or dropped, and reaps
//! the child.
//!
//! The group is part of spawning: the child is in it before it runs, and
//! spawning fails (without leaving the child running) when it can't be put in
//! one, rather than giving a child that's only killed itself.
//!
//! The group also ends with its owner:
//!
//! - Windows: the job kills its processes once it's closed, which the OS does
//!   when the owner exits for any reason.
//! - Linux: the child is killed when the owner's thread that spawned it exits
//!   (`PR_SET_PDEATHSIG`), so spawn from a thread that outlives the child.
//!   [`OwnedChild::spawn_untied`] leaves that out for the many short children
//!   of a hot path, as it makes spawning slower.
//! - unix: a terminal's Ctrl+C no longer reaches a child in a process group of
//!   its own, so an owner that's interrupted should call
//!   [`kill_all_owned_children`] (the dprint CLI does on SIGINT, SIGTERM,
//!   SIGHUP and SIGQUIT).
//!
//! On unix, what a child started can outlive an owner that's killed with
//! SIGKILL, since nothing runs when that happens.

use std::io;
use std::process::Child;
use std::process::ChildStderr;
use std::process::ChildStdin;
use std::process::ChildStdout;
use std::process::Command;
use std::process::ExitStatus;

/// A child process that's killed, together with every process it started,
/// when this is dropped (see the module docs).
///
/// It only gives access to the child through its own methods, so nothing can
/// end the child's life outside of what it keeps track of.
pub struct OwnedChild {
  child: Child,
  group: sys::Group,
}

impl OwnedChild {
  /// Spawns the command as an owned child.
  ///
  /// On Windows this sets the command's creation flags, replacing any it had.
  pub fn spawn(command: &mut Command) -> io::Result<Self> {
    Self::spawn_with(command, true)
  }

  /// Spawns the command as an owned child that the OS doesn't kill when the
  /// owner dies on Linux. Otherwise it's owned the same way.
  ///
  /// That takes Rust's slower way of spawning (`fork` rather than
  /// `posix_spawn`, ~2ms more per process), so this is for the many short
  /// children of a hot path that end on their own once the owner is gone (ex.
  /// a formatter, whose pipes close).
  pub fn spawn_untied(command: &mut Command) -> io::Result<Self> {
    Self::spawn_with(command, false)
  }

  fn spawn_with(command: &mut Command, tied_to_owner: bool) -> io::Result<Self> {
    sys::prepare(command, tied_to_owner);
    #[allow(clippy::disallowed_methods)] // every owned child is spawned here
    let mut child = command.spawn()?;
    #[cfg(test)]
    test::delay_before_group();
    match sys::Group::new(&child) {
      Ok(group) => Ok(Self { child, group }),
      Err(err) => {
        // it hasn't run yet (see `sys::prepare`), so this is all of it
        let _ = child.kill();
        let _ = child.wait();
        Err(io::Error::new(err.kind(), format!("Could not put the process in a group of its own. {}", err)))
      }
    }
  }

  /// The child's process id.
  pub fn id(&self) -> u32 {
    self.child.id()
  }

  /// Takes the child's stdin, when it was piped and wasn't taken before.
  pub fn take_stdin(&mut self) -> Option<ChildStdin> {
    self.child.stdin.take()
  }

  /// Takes the child's stdout, when it was piped and wasn't taken before.
  pub fn take_stdout(&mut self) -> Option<ChildStdout> {
    self.child.stdout.take()
  }

  /// Takes the child's stderr, when it was piped and wasn't taken before.
  pub fn take_stderr(&mut self) -> Option<ChildStderr> {
    self.child.stderr.take()
  }

  /// Waits for the child to exit. What it started keeps running until this is
  /// killed or dropped.
  pub fn wait(&mut self) -> io::Result<ExitStatus> {
    self.child.wait()
  }

  /// The child's exit status, when it exited. What it started keeps running
  /// until this is killed or dropped.
  pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
    self.child.try_wait()
  }

  /// Kills the child and every process it started, then waits for the child
  /// to exit. Unlike [`Child::kill`], this also reaps a child that already
  /// exited on its own.
  pub fn kill(&mut self) -> io::Result<()> {
    self.group.kill();
    self.child.wait().map(|_| ())
  }
}

impl Drop for OwnedChild {
  fn drop(&mut self) {
    // also when the child exited on its own, as it may have left processes
    // running (ex. a shell's background commands)
    let _ = self.kill();
  }
}

/// Kills every owned child that's still alive, together with what it started.
/// For a process that's about to exit because it was interrupted.
pub fn kill_all_owned_children() {
  sys::kill_all();
}

#[cfg(unix)]
mod sys {
  use std::io;
  use std::process::Child;
  use std::process::Command;
  use std::sync::Mutex;

  /// The process groups of the owned children that are alive.
  static GROUPS: Mutex<Vec<libc::pid_t>> = Mutex::new(Vec::new());

  pub fn prepare(command: &mut Command, tied_to_owner: bool) {
    use std::os::unix::process::CommandExt;

    // a process group of its own, which the child is in before it runs (it's
    // set between fork and exec), and is how the child and everything it
    // starts get killed together
    command.process_group(0);

    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    let _ = tied_to_owner;
    #[cfg(any(target_os = "linux", target_os = "android"))]
    if tied_to_owner {
      // SAFETY: only async-signal-safe calls, and nothing allocates
      let owner = unsafe { libc::getpid() };
      unsafe {
        command.pre_exec(move || {
          if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) == -1 {
            return Err(std::io::Error::last_os_error());
          }
          // the owner may have exited before the call above
          if libc::getppid() != owner {
            libc::_exit(1);
          }
          Ok(())
        });
      }
    }
  }

  pub struct Group(libc::pid_t);

  impl Group {
    pub fn new(child: &Child) -> io::Result<Self> {
      // the child leads its group, so the group's id is the child's
      let id = child.id() as libc::pid_t;
      lock_groups().push(id);
      Ok(Self(id))
    }

    pub fn kill(&self) {
      // SAFETY: a plain system call
      unsafe {
        libc::killpg(self.0, libc::SIGKILL);
      }
    }
  }

  impl Drop for Group {
    fn drop(&mut self) {
      let mut groups = lock_groups();
      if let Some(index) = groups.iter().position(|id| *id == self.0) {
        groups.swap_remove(index);
      }
    }
  }

  pub fn kill_all() {
    for id in lock_groups().iter() {
      // SAFETY: a plain system call
      unsafe {
        libc::killpg(*id, libc::SIGKILL);
      }
    }
  }

  fn lock_groups() -> std::sync::MutexGuard<'static, Vec<libc::pid_t>> {
    GROUPS.lock().unwrap_or_else(|err| err.into_inner())
  }
}

#[cfg(windows)]
mod sys {
  use std::io;
  use std::os::windows::io::AsRawHandle;
  use std::os::windows::process::CommandExt;
  use std::process::Child;
  use std::process::Command;

  use winapi::shared::minwindef::DWORD;
  use winapi::shared::minwindef::FALSE;
  use winapi::shared::minwindef::LPVOID;
  use winapi::um::handleapi::CloseHandle;
  use winapi::um::handleapi::INVALID_HANDLE_VALUE;
  use winapi::um::jobapi2::AssignProcessToJobObject;
  use winapi::um::jobapi2::CreateJobObjectW;
  use winapi::um::jobapi2::SetInformationJobObject;
  use winapi::um::jobapi2::TerminateJobObject;
  use winapi::um::processthreadsapi::OpenThread;
  use winapi::um::processthreadsapi::ResumeThread;
  use winapi::um::tlhelp32::CreateToolhelp32Snapshot;
  use winapi::um::tlhelp32::TH32CS_SNAPTHREAD;
  use winapi::um::tlhelp32::THREADENTRY32;
  use winapi::um::tlhelp32::Thread32First;
  use winapi::um::tlhelp32::Thread32Next;
  use winapi::um::winbase::CREATE_SUSPENDED;
  use winapi::um::winnt::HANDLE;
  use winapi::um::winnt::JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
  use winapi::um::winnt::JOBOBJECT_EXTENDED_LIMIT_INFORMATION;
  use winapi::um::winnt::JobObjectExtendedLimitInformation;
  use winapi::um::winnt::THREAD_SUSPEND_RESUME;

  pub fn prepare(command: &mut Command, _tied_to_owner: bool) {
    // created suspended, so it can't run (and start processes) before it's in
    // its job (see `Group::new`), and the job kills its processes when the
    // owner exits for any reason
    command.creation_flags(CREATE_SUSPENDED);
  }

  /// The job the child and every process it starts are in.
  pub struct Group(Handle);

  impl Group {
    /// Puts the child, which was created suspended, in a job of its own, then
    /// lets it run.
    pub fn new(child: &Child) -> io::Result<Self> {
      // SAFETY: the handles are open, and the info has the size passed
      unsafe {
        let job = Handle::new(CreateJobObjectW(std::ptr::null_mut(), std::ptr::null()))?;
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        // closing the job, which also happens when this process exits for any
        // reason, kills every process in it
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if SetInformationJobObject(
          job.0,
          JobObjectExtendedLimitInformation,
          &mut info as *mut _ as LPVOID,
          std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as DWORD,
        ) == 0
        {
          return Err(io::Error::last_os_error());
        }
        if AssignProcessToJobObject(job.0, child.as_raw_handle() as HANDLE) == 0 {
          return Err(io::Error::last_os_error());
        }
        resume_process(child.id())?;
        Ok(Self(job))
      }
    }

    pub fn kill(&self) {
      // SAFETY: the job handle is open until this is dropped
      unsafe {
        TerminateJobObject(self.0.0, 1);
      }
    }
  }

  /// Resumes the threads of a process that was created suspended, which is
  /// its primary thread. (Rust's `Child` keeps that thread's handle, but only
  /// gives it out on nightly.)
  fn resume_process(process_id: DWORD) -> io::Result<()> {
    // SAFETY: the snapshot and thread handles are open where they're used, and
    // the entry has the size it says
    unsafe {
      let snapshot = Handle::new(CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0))?;
      let mut entry: THREADENTRY32 = std::mem::zeroed();
      entry.dwSize = std::mem::size_of::<THREADENTRY32>() as DWORD;
      let mut resumed = 0;
      let mut has_entry = Thread32First(snapshot.0, &mut entry) != 0;
      while has_entry {
        if entry.th32OwnerProcessID == process_id {
          let thread = Handle::new(OpenThread(THREAD_SUSPEND_RESUME, FALSE, entry.th32ThreadID))?;
          if ResumeThread(thread.0) == DWORD::MAX {
            return Err(io::Error::last_os_error());
          }
          resumed += 1;
        }
        entry.dwSize = std::mem::size_of::<THREADENTRY32>() as DWORD;
        has_entry = Thread32Next(snapshot.0, &mut entry) != 0;
      }
      if resumed == 0 {
        return Err(io::Error::other("Could not find the process's thread to start it."));
      }
      Ok(())
    }
  }

  /// A handle that's closed when this is dropped.
  struct Handle(HANDLE);

  // SAFETY: a job handle can be used from any thread
  unsafe impl Send for Handle {}
  unsafe impl Sync for Handle {}

  impl Handle {
    /// Takes a handle a function returned, or the error it failed with.
    fn new(handle: HANDLE) -> io::Result<Self> {
      if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        Err(io::Error::last_os_error())
      } else {
        Ok(Self(handle))
      }
    }
  }

  impl Drop for Handle {
    fn drop(&mut self) {
      // SAFETY: the handle is open, and closed only here
      unsafe {
        CloseHandle(self.0);
      }
    }
  }

  pub fn kill_all() {
    // the jobs kill their processes when this process exits
  }
}

#[cfg(test)]
mod test {
  use std::path::Path;
  use std::path::PathBuf;
  use std::process::Stdio;
  use std::sync::atomic::AtomicU64;
  use std::sync::atomic::Ordering;
  use std::time::Duration;
  use std::time::Instant;

  use super::*;

  /// How long spawning waits between creating the child and putting it in its
  /// group, to show the child doesn't run (and start processes) before then.
  static DELAY_BEFORE_GROUP_MS: AtomicU64 = AtomicU64::new(0);

  pub fn delay_before_group() {
    std::thread::sleep(Duration::from_millis(DELAY_BEFORE_GROUP_MS.load(Ordering::Relaxed)));
  }

  /// The tests spawn and kill children that `kill_all_owned_children` and the
  /// delay affect, so they run one at a time.
  fn serial() -> std::sync::MutexGuard<'static, ()> {
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    SERIAL.lock().unwrap_or_else(|err| err.into_inner())
  }

  fn wait_until(condition: impl Fn() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(10) {
      if condition() {
        return true;
      }
      std::thread::sleep(Duration::from_millis(50));
    }
    false
  }

  /// A file that a process started by a child appends to about every 100ms
  /// (unix) or second (Windows) as long as it runs.
  struct Heartbeat {
    dir: PathBuf,
  }

  impl Drop for Heartbeat {
    fn drop(&mut self) {
      let _ = std::fs::remove_dir_all(&self.dir);
    }
  }

  impl Heartbeat {
    fn new() -> Self {
      static COUNT: AtomicU64 = AtomicU64::new(0);
      let dir = std::env::temp_dir().join(format!("dprint-owned-child-{}-{}", std::process::id(), COUNT.fetch_add(1, Ordering::Relaxed)));
      std::fs::create_dir_all(&dir).unwrap();
      Self { dir }
    }

    fn path(&self) -> PathBuf {
      self.dir.join("heartbeat.txt")
    }

    fn len(&self) -> u64 {
      std::fs::metadata(self.path()).map(|metadata| metadata.len()).unwrap_or(0)
    }

    fn is_beating(&self) -> bool {
      let len = self.len();
      wait_until(|| self.len() > len)
    }

    /// Whether it stopped, once a beat that was underway had time to finish.
    fn stopped(&self) -> bool {
      std::thread::sleep(Duration::from_millis(500));
      let len = self.len();
      std::thread::sleep(Duration::from_millis(2500));
      self.len() == len
    }

    /// A command that right away starts a process that beats, then waits
    /// (longer than any test).
    fn command_starting_it(&self) -> Command {
      command_starting_heartbeat(&self.dir, &self.path(), true)
    }

    /// A command that right away starts a process that beats, then exits.
    fn command_starting_it_and_exiting(&self) -> Command {
      command_starting_heartbeat(&self.dir, &self.path(), false)
    }
  }

  #[cfg(unix)]
  fn command_starting_heartbeat(_dir: &Path, path: &Path, then_wait: bool) -> Command {
    let mut command = Command::new("sh");
    command
      .arg("-c")
      .arg(format!(
        "(while :; do echo x >> '{}'; sleep 0.1; done) & {}",
        path.display(),
        if then_wait { "sleep 600" } else { "exit 0" }
      ))
      .stdin(Stdio::null())
      .stdout(Stdio::null());
    command
  }

  #[cfg(windows)]
  fn command_starting_heartbeat(dir: &Path, path: &Path, then_wait: bool) -> Command {
    use std::os::windows::process::CommandExt;

    let script = dir.join("heartbeat.cmd");
    std::fs::write(
      &script,
      format!(
        "@echo off\r\n:beat\r\necho x>>\"{}\"\r\nping -n 2 127.0.0.1 >nul\r\ngoto beat\r\n",
        path.display()
      ),
    )
    .unwrap();
    let mut command = Command::new("cmd");
    command
      .arg("/c")
      .raw_arg(format!(
        "start \"\" /b \"{}\"{}",
        script.display(),
        if then_wait { " & ping -n 600 127.0.0.1 >nul" } else { "" }
      ))
      .stdin(Stdio::null())
      .stdout(Stdio::null());
    command
  }

  #[test]
  fn kills_what_the_child_started_when_dropped() {
    let _serial = serial();
    let heartbeat = Heartbeat::new();
    let child = OwnedChild::spawn(&mut heartbeat.command_starting_it()).unwrap();
    assert!(heartbeat.is_beating());
    drop(child);
    assert!(heartbeat.stopped());
  }

  #[test]
  fn kill_kills_what_the_child_started_and_reaps_it() {
    let _serial = serial();
    let heartbeat = Heartbeat::new();
    let mut child = OwnedChild::spawn_untied(&mut heartbeat.command_starting_it()).unwrap();
    assert!(heartbeat.is_beating());
    child.kill().unwrap();
    // reaped, so it's no longer a zombie waiting on its owner
    assert!(child.try_wait().unwrap().is_some());
    assert!(heartbeat.stopped());
  }

  #[test]
  fn owns_what_the_child_starts_before_its_group_is_set_up() {
    let _serial = serial();
    // a child that ran before it was in its group would have started the
    // heartbeat outside of it by then
    DELAY_BEFORE_GROUP_MS.store(1500, Ordering::Relaxed);
    let heartbeat = Heartbeat::new();
    let child = OwnedChild::spawn(&mut heartbeat.command_starting_it());
    DELAY_BEFORE_GROUP_MS.store(0, Ordering::Relaxed);
    let child = child.unwrap();
    assert!(heartbeat.is_beating());
    drop(child);
    assert!(heartbeat.stopped());
  }

  #[test]
  fn kills_what_an_exited_child_left_running() {
    let _serial = serial();
    let heartbeat = Heartbeat::new();
    let mut child = OwnedChild::spawn(&mut heartbeat.command_starting_it_and_exiting()).unwrap();
    assert!(child.wait().unwrap().success());
    assert!(heartbeat.is_beating());
    drop(child);
    assert!(heartbeat.stopped());
  }

  // Windows: the jobs kill their processes when this process exits
  #[cfg(unix)]
  #[test]
  fn kills_all_owned_children() {
    let _serial = serial();
    let first = Heartbeat::new();
    let second = Heartbeat::new();
    let _first = OwnedChild::spawn(&mut first.command_starting_it()).unwrap();
    let _second = OwnedChild::spawn(&mut second.command_starting_it()).unwrap();
    assert!(first.is_beating() && second.is_beating());
    kill_all_owned_children();
    assert!(first.stopped() && second.stopped());
  }

  /// Whether a `sleep <seconds>` process is running.
  #[cfg(target_os = "linux")]
  fn sleep_is_running(seconds: &str) -> bool {
    let output = Command::new("ps").args(["-eo", "args"]).output().unwrap();
    String::from_utf8_lossy(&output.stdout)
      .lines()
      .any(|line| line.trim() == format!("sleep {}", seconds))
  }

  #[cfg(target_os = "linux")]
  #[test]
  fn a_tied_child_dies_with_the_thread_that_spawned_it() {
    let _serial = serial();
    let sleep = |seconds: &str| {
      let mut command = Command::new("sleep");
      command.arg(seconds);
      command
    };
    // the owners never drop the children, as if they were killed
    std::thread::spawn(move || std::mem::forget(OwnedChild::spawn(&mut sleep("4356")).unwrap()))
      .join()
      .unwrap();
    std::thread::spawn(move || std::mem::forget(OwnedChild::spawn_untied(&mut sleep("4357")).unwrap()))
      .join()
      .unwrap();
    assert!(wait_until(|| !sleep_is_running("4356")));
    assert!(sleep_is_running("4357"));
    kill_all_owned_children();
    assert!(wait_until(|| !sleep_is_running("4357")));
  }
}
