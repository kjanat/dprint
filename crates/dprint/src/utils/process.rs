use anyhow::Result;

#[cfg(windows)]
pub fn get_running_pids_by_name(searching_name: &str) -> Result<Vec<u32>> {
  use std::process::Command;
  use std::process::Stdio;

  use anyhow::bail;

  let filter = format!("IMAGENAME eq {}.exe", searching_name);
  let output = Command::new("tasklist")
    .args([
      // csv format
      "/FO",
      "CSV",
      // no header
      "/NH",
      // filter by process name
      "/FI",
      filter.as_str(),
    ])
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .output()?;
  if !output.status.success() {
    bail!("Error getting process names: {}", String::from_utf8(output.stderr)?);
  }
  let stdout = String::from_utf8(output.stdout)?;
  let lines = stdout.lines();

  Ok(
    lines
      .filter_map(|line| line.split(',').nth(1).and_then(|p| p.trim_matches('"').parse::<u32>().ok()))
      .collect(),
  )
}

#[cfg(not(windows))]
pub fn get_running_pids_by_name(searching_name: &str) -> Result<Vec<u32>> {
  use std::process::Command;
  use std::process::Stdio;

  use anyhow::bail;

  let output = Command::new("ps")
    .args([
      "-A", "-o", "pid=", // equals, for no header
      "-o", "comm=", // not cmd, because comm works on mac as well
    ])
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .output()?;
  if !output.status.success() {
    bail!("Error getting process names: {}", String::from_utf8(output.stderr)?);
  }
  let stdout = String::from_utf8(output.stdout)?;
  let lines = stdout.lines();
  Ok(
    lines
      .filter_map(|line| {
        let line = line.trim();
        let first_space = line.find(' ')?;
        let pid = &line[..first_space];
        let command_name = &line[first_space + 1..];
        let pid = pid.parse::<u32>().ok()?;
        if command_name == searching_name || command_name.ends_with(&format!("/{}", searching_name)) {
          Some(pid)
        } else {
          None
        }
      })
      .collect(),
  )
}

#[cfg(windows)]
pub fn kill_process_by_id(pid: u32) -> Result<()> {
  let pid_string = pid.to_string();
  run_command(vec!["taskkill", "/F", "/PID", pid_string.as_str()])
}

#[cfg(not(windows))]
pub fn kill_process_by_id(pid: u32) -> Result<()> {
  let pid_string = pid.to_string();
  run_command(vec!["kill", pid_string.as_str()])
}

fn run_command(mut command: Vec<&str>) -> Result<()> {
  use std::process::Command;
  use std::process::Stdio;

  use anyhow::bail;

  let output = Command::new(command.remove(0))
    .args(command)
    .stdout(Stdio::null())
    .stderr(Stdio::piped())
    .output()?;
  if !output.status.success() {
    bail!("Error: {}", String::from_utf8(output.stderr)?);
  }

  Ok(())
}

#[cfg(test)]
mod test {
  use super::*;

  #[test]
  fn gets_process_ids() {
    let results = get_running_pids_by_name("cargo").unwrap();
    assert!(!results.is_empty());
    let results = get_running_pids_by_name("dprint-testing-not-exists").unwrap();
    assert!(results.is_empty());
  }

  /// Checks with the system's process table that the processes an owned
  /// child started die with it, on every platform the tests run on (dprint-core
  /// only tests this on unix). On Windows this is what a `.cmd` shim does:
  /// `cmd.exe` runs the actual program as a process of its own.
  #[test]
  fn kills_the_processes_an_owned_child_started() {
    use std::process::Command;
    use std::process::Stdio;
    use std::time::Duration;
    use std::time::Instant;

    use dprint_core::owned_child::OwnedChild;
    use sysinfo::Pid;
    use sysinfo::ProcessesToUpdate;
    use sysinfo::System;

    fn wait_until<T>(mut check: impl FnMut() -> Option<T>) -> Option<T> {
      let start = Instant::now();
      while start.elapsed() < Duration::from_secs(10) {
        if let Some(value) = check() {
          return Some(value);
        }
        std::thread::sleep(Duration::from_millis(50));
      }
      None
    }

    let mut command = if cfg!(windows) {
      let mut command = Command::new("cmd");
      command.args(["/c", "ping -n 61 127.0.0.1 > nul"]);
      command
    } else {
      let mut command = Command::new("sh");
      command.args(["-c", "sleep 61; true"]);
      command
    };
    command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    let child = OwnedChild::spawn(&mut command).unwrap();
    let child_pid = Pid::from_u32(child.id());

    let mut system = System::new();
    let started = wait_until(|| {
      system.refresh_processes(ProcessesToUpdate::All, true);
      system
        .processes()
        .iter()
        .find(|(_, process)| process.parent() == Some(child_pid))
        .map(|(pid, _)| *pid)
    })
    .expect("the child should start a process");

    drop(child);
    let ended = wait_until(|| {
      system.refresh_processes(ProcessesToUpdate::Some(&[started]), true);
      system.process(started).is_none().then_some(())
    });
    assert!(ended.is_some(), "the process the child started is still running");
  }
}
