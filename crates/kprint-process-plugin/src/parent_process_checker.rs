use std::time::Duration;

/// Starts a task that polls for the existence of the parent process.
/// If the parent process no longer exists, then it will exit the current process.
///
/// Note: This must be called from a tokio runtime.
pub fn start_parent_process_checker_task(parent_process_id: u32) {
  kprint_async_runtime::spawn(async move {
    // wait cheaply for 2 seconds
    tokio::time::sleep(Duration::from_secs(2)).await;

    // now spawn a dedicated thread that will keep checking this
    // in case the tokio runtime gets blocked. Also, DO NOT
    // use spawn_blocking from tokio here because it will keep
    // the process alive
    std::thread::spawn(move || {
      loop {
        std::thread::sleep(Duration::from_secs(5));
        if !is_process_active(parent_process_id) {
          std::process::exit(1);
        }
      }
    });
  });
}

/// Gets the parent process id from the CLI arguments.
///
/// The dprint cli will provide a `--parent-pid <value>` flag to specify the parent PID.
pub fn get_parent_process_id_from_cli_args() -> Option<u32> {
  let args: Vec<String> = std::env::args().collect();
  for i in 0..args.len() {
    if args[i] == "--parent-pid"
      && let Some(parent_pid) = args.get(i + 1)
    {
      return parent_pid.parse::<u32>().map(Some).unwrap_or(None);
    }
  }

  None
}

// code below is from my implementation when adding this to Deno

#[cfg(unix)]
fn is_process_active(process_id: u32) -> bool {
  unsafe {
    // signal of 0 checks for the existence of the process id
    libc::kill(process_id as i32, 0) == 0
  }
}

#[cfg(windows)]
fn is_process_active(process_id: u32) -> bool {
  use winapi::shared::minwindef::DWORD;
  use winapi::shared::minwindef::FALSE;
  use winapi::shared::ntdef::NULL;
  use winapi::shared::winerror::WAIT_TIMEOUT;
  use winapi::um::handleapi::CloseHandle;
  use winapi::um::processthreadsapi::OpenProcess;
  use winapi::um::synchapi::WaitForSingleObject;
  use winapi::um::winnt::SYNCHRONIZE;

  unsafe {
    let process = OpenProcess(SYNCHRONIZE, FALSE, process_id as DWORD);
    let result = if process == NULL {
      false
    } else {
      WaitForSingleObject(process, 0) == WAIT_TIMEOUT
    };

    CloseHandle(process);
    result
  }
}

#[cfg(test)]
mod test {
  use super::is_process_active;
  use kprint_owned_child::OwnedChild;
  use std::process::Command;
  use std::process::Stdio;

  #[test]
  fn should_tell_when_process_active() {
    // launch a long running process, one every system has, so the test
    // doesn't need anything built first
    let mut command = if cfg!(windows) {
      let mut command = Command::new("ping");
      command.args(["-n", "600", "127.0.0.1"]);
      command
    } else {
      let mut command = Command::new("sleep");
      command.arg("600");
      command
    };
    let mut child = OwnedChild::spawn(command.stdout(Stdio::null())).unwrap();

    let pid = child.id();
    assert!(is_process_active(pid));
    // also waits for it to exit
    child.kill().unwrap();
    assert!(!is_process_active(pid));
  }
}
