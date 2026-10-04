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
use std::ops::Deref;
use std::ops::DerefMut;
use std::process::Child;
use std::process::Command;

/// A child process that's killed, together with every process it started,
/// when this is dropped (see the module docs).
pub struct OwnedChild {
  child: Child,
  group: sys::Group,
}

impl OwnedChild {
  /// Spawns the command as an owned child.
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
    let child = command.spawn()?;
    let group = sys::Group::new(&child);
    Ok(Self { child, group })
  }

  /// Kills the child and every process it started, then waits for the child
  /// to exit. Unlike [`Child::kill`], this also reaps a child that already
  /// exited on its own.
  pub fn kill(&mut self) -> io::Result<()> {
    self.group.kill();
    // for a child that isn't in a group (see `sys::Group`)
    let _ = self.child.kill();
    self.child.wait().map(|_| ())
  }
}

impl Deref for OwnedChild {
  type Target = Child;

  fn deref(&self) -> &Child {
    &self.child
  }
}

impl DerefMut for OwnedChild {
  fn deref_mut(&mut self) -> &mut Child {
    &mut self.child
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
  use std::process::Child;
  use std::process::Command;
  use std::sync::Mutex;

  /// The process groups of the owned children that are alive.
  static GROUPS: Mutex<Vec<libc::pid_t>> = Mutex::new(Vec::new());

  pub fn prepare(command: &mut Command, tied_to_owner: bool) {
    use std::os::unix::process::CommandExt;

    // a process group of its own, which is how the child and everything it
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
    pub fn new(child: &Child) -> Self {
      // the child leads its group, so the group's id is the child's
      let id = child.id() as libc::pid_t;
      lock_groups().push(id);
      Self(id)
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
  use std::os::windows::io::AsRawHandle;
  use std::process::Child;
  use std::process::Command;

  use winapi::shared::minwindef::DWORD;
  use winapi::shared::minwindef::LPVOID;
  use winapi::um::handleapi::CloseHandle;
  use winapi::um::jobapi2::AssignProcessToJobObject;
  use winapi::um::jobapi2::CreateJobObjectW;
  use winapi::um::jobapi2::SetInformationJobObject;
  use winapi::um::jobapi2::TerminateJobObject;
  use winapi::um::winnt::HANDLE;
  use winapi::um::winnt::JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
  use winapi::um::winnt::JOBOBJECT_EXTENDED_LIMIT_INFORMATION;
  use winapi::um::winnt::JobObjectExtendedLimitInformation;

  pub fn prepare(_command: &mut Command, _tied_to_owner: bool) {
    // the job kills its processes when the owner exits for any reason
  }

  /// The job the child is in. A child that couldn't be put in one (ex. the
  /// system doesn't allow it) is only killed itself.
  ///
  /// The child is put in the job right after it's created, so a process it
  /// started in the meantime isn't in it. A process starts in milliseconds,
  /// so in practice that's nothing.
  pub struct Group(Option<HANDLE>);

  // SAFETY: a job handle can be used from any thread
  unsafe impl Send for Group {}
  unsafe impl Sync for Group {}

  impl Group {
    pub fn new(child: &Child) -> Self {
      // SAFETY: the handles are valid, and the info has the size passed
      unsafe {
        let job = CreateJobObjectW(std::ptr::null_mut(), std::ptr::null());
        if job.is_null() {
          return Self(None);
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        // closing the job, which also happens when this process exits for any
        // reason, kills every process in it
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let is_in_job = SetInformationJobObject(
          job,
          JobObjectExtendedLimitInformation,
          &mut info as *mut _ as LPVOID,
          std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as DWORD,
        ) != 0
          && AssignProcessToJobObject(job, child.as_raw_handle() as HANDLE) != 0;
        if !is_in_job {
          CloseHandle(job);
          return Self(None);
        }
        Self(Some(job))
      }
    }

    pub fn kill(&self) {
      if let Some(job) = self.0 {
        // SAFETY: the job handle is open until this is dropped
        unsafe {
          TerminateJobObject(job, 1);
        }
      }
    }
  }

  impl Drop for Group {
    fn drop(&mut self) {
      if let Some(job) = self.0.take() {
        // SAFETY: the job handle is open, and closed only here
        unsafe {
          CloseHandle(job);
        }
      }
    }
  }

  pub fn kill_all() {
    // the jobs kill their processes when this process exits
  }
}

#[cfg(all(test, unix))]
mod test {
  use std::process::Stdio;
  use std::time::Duration;
  use std::time::Instant;

  use super::*;

  /// Whether a `sleep <seconds>` process is running.
  fn sleep_is_running(seconds: &str) -> bool {
    let output = Command::new("ps").args(["-eo", "args"]).output().unwrap();
    String::from_utf8_lossy(&output.stdout)
      .lines()
      .any(|line| line.trim() == format!("sleep {}", seconds))
  }

  fn wait_until(condition: impl Fn() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(5) {
      if condition() {
        return true;
      }
      std::thread::sleep(Duration::from_millis(20));
    }
    false
  }

  /// `kill_all_owned_children` kills the children of every test, so the tests
  /// run one at a time.
  fn serial() -> std::sync::MutexGuard<'static, ()> {
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    SERIAL.lock().unwrap_or_else(|err| err.into_inner())
  }

  fn sh(script: &str) -> Command {
    let mut command = Command::new("sh");
    command.args(["-c", script]).stdin(Stdio::null()).stdout(Stdio::null());
    command
  }

  #[test]
  fn kills_what_the_child_started_when_dropped() {
    let _serial = serial();
    // the shell starts `sleep` as a process of its own
    let child = OwnedChild::spawn(&mut sh("sleep 4351; true")).unwrap();
    assert!(wait_until(|| sleep_is_running("4351")));
    drop(child);
    assert!(wait_until(|| !sleep_is_running("4351")));
  }

  #[test]
  fn kills_what_an_exited_child_left_running() {
    let _serial = serial();
    let mut child = OwnedChild::spawn(&mut sh("sleep 4352 &")).unwrap();
    assert!(child.wait().unwrap().success());
    assert!(wait_until(|| sleep_is_running("4352")));
    drop(child);
    assert!(wait_until(|| !sleep_is_running("4352")));
  }

  #[test]
  fn kill_kills_what_the_child_started_and_reaps_it() {
    let _serial = serial();
    let mut child = OwnedChild::spawn(&mut sh("sleep 4353; true")).unwrap();
    assert!(wait_until(|| sleep_is_running("4353")));
    child.kill().unwrap();
    // reaped, so it's no longer a zombie waiting on its owner
    assert!(child.try_wait().unwrap().is_some());
    assert!(wait_until(|| !sleep_is_running("4353")));
  }

  #[cfg(target_os = "linux")]
  #[test]
  fn a_tied_child_dies_with_the_thread_that_spawned_it() {
    let _serial = serial();
    // the owners never drop the children, as if they were killed
    std::thread::spawn(|| std::mem::forget(OwnedChild::spawn(&mut sh("exec sleep 4356")).unwrap()))
      .join()
      .unwrap();
    std::thread::spawn(|| std::mem::forget(OwnedChild::spawn_untied(&mut sh("exec sleep 4357")).unwrap()))
      .join()
      .unwrap();
    assert!(wait_until(|| !sleep_is_running("4356")));
    assert!(sleep_is_running("4357"));
    kill_all_owned_children();
    assert!(wait_until(|| !sleep_is_running("4357")));
  }

  #[test]
  fn kills_all_owned_children() {
    let _serial = serial();
    let _first = OwnedChild::spawn(&mut sh("sleep 4354; true")).unwrap();
    let _second = OwnedChild::spawn(&mut sh("sleep 4355; true")).unwrap();
    assert!(wait_until(|| sleep_is_running("4354") && sleep_is_running("4355")));
    kill_all_owned_children();
    assert!(wait_until(|| !sleep_is_running("4354") && !sleep_is_running("4355")));
  }
}
