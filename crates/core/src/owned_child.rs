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
//! What the group holds:
//!
//! - Windows: every process the child starts, and what those start, as a job
//!   doesn't let its processes break away unless it allows that, which this
//!   one doesn't.
//! - unix: every process the child starts, and what those start, unless one
//!   leaves the process group (`setsid` or `setpgid`). That's how a daemon
//!   detaches (ex. a formatter's server started with Node's
//!   `detached: true`, which is meant to outlive the command that started
//!   it), so such a process isn't owned.
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

use std::cell::Cell;
use std::io;
use std::process::Child;
use std::process::ChildStderr;
use std::process::ChildStdin;
use std::process::ChildStdout;
use std::process::Command;
use std::process::ExitStatus;
use std::sync::Condvar;
use std::sync::Mutex;
use std::sync::MutexGuard;

/// A child process that's killed, together with every process it started,
/// when this is dropped (see the module docs).
///
/// It only gives access to the child through its own methods, so nothing can
/// end the child's life outside of what it keeps track of.
pub struct OwnedChild {
  child: Child,
  /// The child's group, until the child is reaped (see [`OwnedChild::kill`]).
  /// On unix the group's id is the child's, which the OS may give to another
  /// process once the child is reaped, so the group is retired then.
  group: Option<sys::Group>,
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
    let mut spawning = Spawning::start()?;
    sys::prepare(command, tied_to_owner);
    #[cfg(test)]
    test::while_creating();
    #[allow(clippy::disallowed_methods)] // every owned child is spawned here
    let mut child = command.spawn()?;
    spawning.created(&child);
    #[cfg(test)]
    test::delay_before_group();
    let group = sys::Group::new(&child);
    // in its group now, or failed to be, so it's no longer one for
    // `kill_all_owned_children` to kill on its own. That's before it's reaped
    // below, after which its id may be another process's
    let owned_children_were_killed = spawning.finish();
    let group = match group {
      Ok(group) => group,
      Err(err) => {
        // it hasn't run yet (see `sys::prepare`), so this is all of it
        let _ = child.kill();
        let _ = child.wait();
        if owned_children_were_killed {
          return Err(owned_children_killed_error());
        }
        return Err(io::Error::new(err.kind(), format!("Could not put the process in a group of its own. {}", err)));
      }
    };
    let mut owned = Self { child, group: Some(group) };
    if owned_children_were_killed {
      // as the process is about to exit
      let _ = owned.kill();
      return Err(owned_children_killed_error());
    }
    Ok(owned)
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
  /// killed or dropped, and on unix the child isn't reaped until then, so
  /// that its group's id can't be reused before the group is killed.
  pub fn wait(&mut self) -> io::Result<ExitStatus> {
    if self.group.is_none() {
      return self.child.wait();
    }
    sys::wait(&mut self.child, true)?.ok_or_else(|| io::Error::other("The process exited without a status."))
  }

  /// The child's exit status, when it exited. What it started keeps running
  /// until this is killed or dropped, and on unix the child isn't reaped
  /// until then (see [`OwnedChild::wait`]).
  pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
    if self.group.is_none() {
      return self.child.try_wait();
    }
    sys::wait(&mut self.child, false)
  }

  /// Kills the child and every process it started, then waits for the child
  /// to exit and reaps it. Unlike [`Child::kill`], this also reaps a child
  /// that already exited on its own.
  ///
  /// When the group can't be killed (the OS refuses), that's the error, and
  /// the child isn't waited for, as it may well go on running. The group is
  /// kept, so that killing it again (ex. when this is dropped, or by
  /// [`kill_all_owned_children`]) tries once more.
  pub fn kill(&mut self) -> io::Result<()> {
    if let Some(group) = &self.group {
      group.kill()?;
      // retired before the child is reaped below, after which its id may be
      // another process's (see `group`)
      self.group = None;
    }
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

/// Kills every owned child that's still alive, together with what it started,
/// and keeps any more from being spawned. For a process that's about to exit
/// because it was interrupted or failed: once this returns, every owned child
/// is dead or dies with the process (Windows), including ones being spawned.
pub fn kill_all_owned_children() {
  let mut state = lock_spawn_state();
  state.owned_children_killed = true;
  // created, but not in their group yet (ex. on Windows, suspended and not in
  // the job that would kill them when this process exits)
  for child in &state.not_in_group {
    child.kill();
  }
  drop(state);
  sys::kill_all();
  // a child the OS is still creating is killed once it's created (see
  // `Spawning::created`), which this waits for, however long that takes. Not
  // for one this thread is creating, as that can't go on meanwhile (ex. when
  // a panic while spawning has this called)
  let creating_here = CREATING_HERE.with(Cell::get);
  let state = lock_spawn_state();
  drop(SPAWNED.wait_while(state, |state| state.creating > creating_here));
}

fn owned_children_killed_error() -> io::Error {
  io::Error::other("Did not start the process, as the owned processes were killed.")
}

/// What [`kill_all_owned_children`] knows about spawning.
struct SpawnState {
  /// How many owned children the OS is creating.
  creating: usize,
  /// The owned children that were created but aren't in their group yet.
  not_in_group: Vec<sys::CreatedChild>,
  /// Whether [`kill_all_owned_children`] was called.
  owned_children_killed: bool,
}

static SPAWN_STATE: Mutex<SpawnState> = Mutex::new(SpawnState {
  creating: 0,
  not_in_group: Vec::new(),
  owned_children_killed: false,
});
/// Notified when an owned child is done being created.
static SPAWNED: Condvar = Condvar::new();

thread_local! {
  /// How many of the owned children the OS is creating this thread creates.
  static CREATING_HERE: Cell<usize> = const { Cell::new(0) };
}

fn lock_spawn_state() -> MutexGuard<'static, SpawnState> {
  SPAWN_STATE.lock().unwrap_or_else(|err| err.into_inner())
}

/// An owned child being spawned, which [`kill_all_owned_children`] kills
/// before it's in its group.
struct Spawning {
  stage: SpawningStage,
}

enum SpawningStage {
  /// The OS is creating it.
  Creating,
  /// It was created, but isn't in its group yet.
  NotInGroup(sys::CreatedChild),
  /// It's in its group, or failed to be.
  Finished,
}

impl Spawning {
  fn start() -> io::Result<Self> {
    let mut state = lock_spawn_state();
    if state.owned_children_killed {
      return Err(owned_children_killed_error());
    }
    state.creating += 1;
    CREATING_HERE.with(|count| count.set(count.get() + 1));
    Ok(Self {
      stage: SpawningStage::Creating,
    })
  }

  /// The child was created, so it's killed if the owned children are or were.
  fn created(&mut self, child: &Child) {
    let child = sys::CreatedChild::of(child);
    let mut state = lock_spawn_state();
    if state.owned_children_killed {
      child.kill();
    } else {
      state.not_in_group.push(child);
    }
    self.done_creating(&mut state);
    self.stage = SpawningStage::NotInGroup(child);
  }

  /// The child is in its group, or failed to be. Says whether the owned
  /// children were killed since this started.
  fn finish(&mut self) -> bool {
    let mut state = lock_spawn_state();
    if let SpawningStage::NotInGroup(child) = std::mem::replace(&mut self.stage, SpawningStage::Finished)
      && let Some(index) = state.not_in_group.iter().position(|created| *created == child)
    {
      state.not_in_group.swap_remove(index);
    }
    state.owned_children_killed
  }

  fn done_creating(&self, state: &mut SpawnState) {
    state.creating -= 1;
    CREATING_HERE.with(|count| count.set(count.get() - 1));
    SPAWNED.notify_all();
  }
}

impl Drop for Spawning {
  fn drop(&mut self) {
    match self.stage {
      // the OS failed to create it
      SpawningStage::Creating => self.done_creating(&mut lock_spawn_state()),
      SpawningStage::NotInGroup(_) => {
        self.finish();
      }
      SpawningStage::Finished => {}
    }
  }
}

#[cfg(unix)]
mod sys {
  use std::io;
  use std::process::Child;
  use std::process::Command;
  use std::process::ExitStatus;
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

  /// A child that was created and isn't reaped yet, by its id, which is its
  /// process group's (see `prepare`).
  #[derive(Clone, Copy, PartialEq)]
  pub struct CreatedChild(libc::pid_t);

  impl CreatedChild {
    pub fn of(child: &Child) -> Self {
      Self(child.id() as libc::pid_t)
    }

    /// Kills it and what it started.
    pub fn kill(&self) {
      // SAFETY: a plain system call
      unsafe {
        libc::killpg(self.0, libc::SIGKILL);
      }
    }

    /// Whether it exited, without reaping it.
    #[cfg(test)]
    pub fn has_exited(&self) -> bool {
      // SAFETY: an all zero siginfo_t is valid, and is what's left when there
      // was nothing to report
      let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
      // SAFETY: a plain system call with a pointer to the struct it fills in
      let result = unsafe { libc::waitid(libc::P_PID, self.0 as libc::id_t, &mut info, libc::WEXITED | libc::WNOWAIT | libc::WNOHANG) };
      // SAFETY: waitid filled in the child's exit, or nothing
      result == 0 && unsafe { info.si_pid() } != 0
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

    /// Kills every process in the group. Errors when the OS refuses (ex. a
    /// process in it that this process may not signal), in which case they
    /// may go on running.
    pub fn kill(&self) -> io::Result<()> {
      #[cfg(test)]
      super::test::fail_killing_group()?;
      // SAFETY: a plain system call
      if unsafe { libc::killpg(self.0, libc::SIGKILL) } == -1 {
        let err = io::Error::last_os_error();
        // no process left in it is as killed as it gets
        if err.raw_os_error() != Some(libc::ESRCH) {
          return Err(err);
        }
      }
      Ok(())
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

  /// The child's exit status once it exited, without reaping it: until it's
  /// reaped, its id, which is its group's, isn't given to another process.
  pub fn wait(child: &mut Child, block: bool) -> io::Result<Option<ExitStatus>> {
    use std::os::unix::process::ExitStatusExt;

    let options = libc::WEXITED | libc::WNOWAIT | if block { 0 } else { libc::WNOHANG };
    loop {
      // SAFETY: an all zero siginfo_t is valid, and is what's left when there
      // was nothing to report
      let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
      // SAFETY: a plain system call with a pointer to the struct it fills in
      if unsafe { libc::waitid(libc::P_PID, child.id() as libc::id_t, &mut info, options) } == -1 {
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::Interrupted {
          continue;
        }
        return Err(err);
      }
      // SAFETY: waitid filled in the child's exit, or nothing
      let (pid, status) = unsafe { (info.si_pid(), info.si_status()) };
      if pid == 0 {
        // still running
        return Ok(None);
      }
      // the status as `waitpid` reports it
      let raw_status = match info.si_code {
        libc::CLD_KILLED => status,
        libc::CLD_DUMPED => status | 0x80,
        _ => (status & 0xff) << 8,
      };
      return Ok(Some(ExitStatus::from_raw(raw_status)));
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
  use std::process::ExitStatus;

  use winapi::shared::minwindef::BOOL;
  use winapi::shared::minwindef::DWORD;
  use winapi::shared::minwindef::FALSE;
  use winapi::shared::minwindef::LPVOID;
  use winapi::um::handleapi::CloseHandle;
  use winapi::um::handleapi::INVALID_HANDLE_VALUE;
  use winapi::um::jobapi::IsProcessInJob;
  use winapi::um::jobapi2::AssignProcessToJobObject;
  use winapi::um::jobapi2::CreateJobObjectW;
  use winapi::um::jobapi2::SetInformationJobObject;
  use winapi::um::jobapi2::TerminateJobObject;
  use winapi::um::processthreadsapi::GetCurrentProcess;
  use winapi::um::processthreadsapi::OpenThread;
  use winapi::um::processthreadsapi::ResumeThread;
  use winapi::um::processthreadsapi::TerminateProcess;
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

  /// A child that was created, by its process handle, which `Child` keeps
  /// open until it's dropped, which happens after this is (see `Spawning`).
  #[derive(Clone, Copy, PartialEq)]
  pub struct CreatedChild(usize);

  impl CreatedChild {
    pub fn of(child: &Child) -> Self {
      Self(child.as_raw_handle() as usize)
    }

    /// Kills it. It's suspended until it's in its job (see `prepare`), so it
    /// didn't start anything.
    pub fn kill(&self) {
      // SAFETY: the process handle is open (see the type's docs)
      unsafe {
        TerminateProcess(self.0 as HANDLE, 1);
      }
    }

    /// Whether it exited.
    #[cfg(test)]
    pub fn has_exited(&self) -> bool {
      // SAFETY: the process handle is open (see the type's docs)
      unsafe { winapi::um::synchapi::WaitForSingleObject(self.0 as HANDLE, 0) == winapi::um::winbase::WAIT_OBJECT_0 }
    }
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
          let err = io::Error::last_os_error();
          if is_in_job() {
            // the child is in this process's job too, which may not allow
            // jobs within it
            return Err(io::Error::new(
              err.kind(),
              format!(
                "{} This process runs in a job, which may not allow jobs within it (ex. when it has UI restrictions).",
                err
              ),
            ));
          }
          return Err(err);
        }
        resume_process(child.id())?;
        Ok(Self(job))
      }
    }

    /// Kills every process in the job. Errors when the OS refuses, in which
    /// case they may go on running.
    pub fn kill(&self) -> io::Result<()> {
      #[cfg(test)]
      super::test::fail_killing_group()?;
      // SAFETY: the job handle is open until this is dropped
      if unsafe { TerminateJobObject(self.0.0, 1) } == 0 {
        return Err(io::Error::last_os_error());
      }
      Ok(())
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

  /// Whether this process runs in a job.
  fn is_in_job() -> bool {
    let mut in_job: BOOL = FALSE;
    // SAFETY: the pseudo handle of this process, and a pointer to the result
    unsafe { IsProcessInJob(GetCurrentProcess(), std::ptr::null_mut(), &mut in_job) != 0 && in_job != FALSE }
  }

  pub fn wait(child: &mut Child, block: bool) -> io::Result<Option<ExitStatus>> {
    // the job's handle, rather than an id, refers to the group
    if block { child.wait().map(Some) } else { child.try_wait() }
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

  thread_local! {
    /// Whether spawning on this thread kills all owned children while the OS
    /// is creating the child, like a panic hook would.
    static KILL_ALL_WHILE_CREATING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
  }

  pub fn while_creating() {
    if KILL_ALL_WHILE_CREATING.with(|kill| kill.get()) {
      kill_all_owned_children();
    }
  }

  thread_local! {
    /// Whether killing a group on this thread fails, the way the OS refusing
    /// to would (the syscall is the only thing left out).
    static FAIL_KILLING_GROUP: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
  }

  pub fn fail_killing_group() -> io::Result<()> {
    if FAIL_KILLING_GROUP.with(|fail| fail.get()) {
      return Err(io::Error::from(io::ErrorKind::PermissionDenied));
    }
    Ok(())
  }

  #[test]
  fn says_when_the_group_cant_be_killed_rather_than_waiting() {
    let _serial = serial();
    let heartbeat = Heartbeat::new();
    let mut child = OwnedChild::spawn(&mut heartbeat.command_starting_it()).unwrap();
    assert!(heartbeat.is_beating());
    FAIL_KILLING_GROUP.with(|fail| fail.set(true));
    let start = Instant::now();
    let err = child.kill().unwrap_err();
    FAIL_KILLING_GROUP.with(|fail| fail.set(false));
    assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
    // right away, rather than waiting for a child that wasn't killed
    assert!(start.elapsed() < Duration::from_secs(1), "{:?}", start.elapsed());
    assert!(heartbeat.is_beating());
    assert!(child.try_wait().unwrap().is_none());
    // the group is kept, so killing it again can work
    child.kill().unwrap();
    assert!(child.try_wait().unwrap().is_some());
    assert!(heartbeat.stopped());
  }

  /// Whether this is the process of its own the test runs in, as it calls
  /// `kill_all_owned_children`, which would kill the owned children of the
  /// other tests running in this one (ex. in other modules) and keep them from
  /// spawning more. Otherwise this runs the test in one and checks it passed.
  fn in_own_process(test_name: &str) -> bool {
    const TEST_ENV_VAR: &str = "DPRINT_OWNED_CHILD_TEST";
    if std::env::var(TEST_ENV_VAR).as_deref() == Ok(test_name) {
      return true;
    }
    // the test's name without the crate's
    let module_path = module_path!().split_once("::").unwrap().1;
    let output = Command::new(std::env::current_exe().unwrap())
      .args([&format!("{}::{}", module_path, test_name), "--exact", "--nocapture"])
      .env(TEST_ENV_VAR, test_name)
      .output()
      .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{}\n{}", stdout, stderr);
    assert!(stdout.contains("1 passed"), "{}\n{}", stdout, stderr);
    false
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
    if !in_own_process("kills_all_owned_children") {
      return;
    }
    let first = Heartbeat::new();
    let second = Heartbeat::new();
    let _first = OwnedChild::spawn(&mut first.command_starting_it()).unwrap();
    let _second = OwnedChild::spawn(&mut second.command_starting_it()).unwrap();
    assert!(first.is_beating() && second.is_beating());
    kill_all_owned_children();
    assert!(first.stopped() && second.stopped());
  }

  #[test]
  fn kills_a_child_spawned_while_killing_all_owned_children() {
    if !in_own_process("kills_a_child_spawned_while_killing_all_owned_children") {
      return;
    }
    let heartbeat = Heartbeat::new();
    let mut command = heartbeat.command_starting_it();
    // untied, as a tied child would die with the thread that spawned it. Its
    // group takes longer than anything here, so what kills the child is
    // `kill_all_owned_children`, not the spawn
    DELAY_BEFORE_GROUP_MS.store(5000, Ordering::Relaxed);
    let spawning = std::thread::spawn(move || OwnedChild::spawn_untied(&mut command));
    // while it's created, but not in its group yet (on Windows, suspended
    // and not in the job that kills it when this process exits)
    assert!(wait_until(|| !lock_spawn_state().not_in_group.is_empty()));
    let start = Instant::now();
    kill_all_owned_children();
    let returned_after = start.elapsed();
    // it's dead, before the spawn could do anything about it, so it's not left
    // running (or suspended) once the process exits next
    let deadline = Instant::now() + Duration::from_secs(2);
    let has_exited = || lock_spawn_state().not_in_group.iter().all(|child| child.has_exited());
    while !has_exited() && Instant::now() < deadline {
      std::thread::sleep(Duration::from_millis(20));
    }
    let killed_before_its_group = has_exited() && !lock_spawn_state().not_in_group.is_empty();
    DELAY_BEFORE_GROUP_MS.store(0, Ordering::Relaxed);
    // the spawn failed rather than giving a child that was killed
    let was_spawned = spawning.join().unwrap().is_ok();
    // and none can be spawned after
    let spawned_after = OwnedChild::spawn(&mut heartbeat.command_starting_it()).is_ok();
    assert!(killed_before_its_group);
    assert!(!was_spawned && !spawned_after);
    // without waiting for the spawn to put it in its group
    assert!(returned_after < Duration::from_secs(2), "{:?}", returned_after);
    assert!(heartbeat.stopped());
  }

  #[test]
  fn kills_all_owned_children_while_this_thread_is_creating_one() {
    if !in_own_process("kills_all_owned_children_while_this_thread_is_creating_one") {
      return;
    }
    let heartbeat = Heartbeat::new();
    let mut command = heartbeat.command_starting_it();
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
      // ex. a panic while spawning, whose hook kills all owned children, which
      // can't wait for the child this thread is creating
      KILL_ALL_WHILE_CREATING.with(|kill| kill.set(true));
      let _ = sender.send(OwnedChild::spawn_untied(&mut command).is_ok());
    });
    let was_spawned = receiver.recv_timeout(Duration::from_secs(10));
    assert_eq!(was_spawned, Ok(false));
    // and the child it went on to create was killed
    assert!(heartbeat.stopped());
  }

  #[cfg(unix)]
  #[test]
  fn keeps_the_child_and_so_its_group_id_until_killed() {
    let _serial = serial();
    use std::os::unix::process::ExitStatusExt;
    let exists = |pid: u32| unsafe { libc::kill(pid as libc::pid_t, 0) } == 0;
    let mut child = OwnedChild::spawn(Command::new("sh").args(["-c", "exit 3"])).unwrap();
    let pid = child.id();
    assert_eq!(child.wait().unwrap().code(), Some(3));
    assert_eq!(child.try_wait().unwrap().and_then(|status| status.code()), Some(3));
    // it exited, but isn't reaped, so its id (its group's) can't be another
    // process's yet
    assert!(exists(pid));
    child.kill().unwrap();
    assert!(!exists(pid));
    // and it isn't looked up by its id anymore
    assert_eq!(child.try_wait().unwrap().and_then(|status| status.code()), Some(3));
    assert_eq!(child.wait().unwrap().code(), Some(3));

    // killed by a signal
    let mut child = OwnedChild::spawn(Command::new("sh").args(["-c", "kill -9 $$"])).unwrap();
    let status = child.wait().unwrap();
    assert_eq!((status.code(), status.signal()), (None, Some(9)));
    child.kill().unwrap();
    assert_eq!(child.wait().unwrap().signal(), Some(9));
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
    if !in_own_process("a_tied_child_dies_with_the_thread_that_spawned_it") {
      return;
    }
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
