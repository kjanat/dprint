use std::time::Duration;

/// The CPU time the current thread has used, if the platform says.
///
/// It measures work rather than how long it took: a thread that waited for
/// a CPU (ex. while many other processes run), or for another thread, did no
/// more work. On Windows it's counted in clock ticks (about 15ms), so the
/// difference of two readings is only right on average, over many of them.
pub fn thread_cpu_time() -> Option<Duration> {
  imp::thread_cpu_time()
}

#[cfg(unix)]
mod imp {
  use std::time::Duration;

  pub fn thread_cpu_time() -> Option<Duration> {
    let mut time = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: `time` is a timespec for it to write to
    if unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut time) } != 0 {
      return None;
    }
    Some(Duration::new(u64::try_from(time.tv_sec).ok()?, u32::try_from(time.tv_nsec).ok()?))
  }
}

#[cfg(windows)]
mod imp {
  use std::time::Duration;

  use winapi::shared::minwindef::FILETIME;
  use winapi::um::processthreadsapi::GetCurrentThread;
  use winapi::um::processthreadsapi::GetThreadTimes;

  pub fn thread_cpu_time() -> Option<Duration> {
    let empty = || FILETIME {
      dwLowDateTime: 0,
      dwHighDateTime: 0,
    };
    let (mut creation, mut exit, mut kernel, mut user) = (empty(), empty(), empty(), empty());
    // SAFETY: the current thread's pseudo handle, and FILETIMEs for it to
    // write to
    if unsafe { GetThreadTimes(GetCurrentThread(), &mut creation, &mut exit, &mut kernel, &mut user) } == 0 {
      return None;
    }
    // in 100ns units
    let units = |time: FILETIME| (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime);
    Some(Duration::from_nanos(units(kernel).checked_add(units(user))?.checked_mul(100)?))
  }
}

#[cfg(not(any(unix, windows)))]
mod imp {
  use std::time::Duration;

  pub fn thread_cpu_time() -> Option<Duration> {
    None
  }
}

#[cfg(test)]
mod test {
  use std::time::Duration;
  use std::time::Instant;

  use super::thread_cpu_time;

  #[test]
  fn counts_work_but_not_waiting() {
    let start = thread_cpu_time().unwrap();
    std::thread::sleep(Duration::from_millis(200));
    let slept = thread_cpu_time().unwrap() - start;
    // sleeping is no work (a clock tick at most, on Windows)
    assert!(slept < Duration::from_millis(50), "{:?}", slept);

    let start = thread_cpu_time().unwrap();
    let busy_until = Instant::now() + Duration::from_millis(300);
    let mut spins = 0u64;
    while Instant::now() < busy_until {
      spins = std::hint::black_box(spins.wrapping_add(1));
    }
    let worked = thread_cpu_time().unwrap() - start;
    // most of it, unless the thread waited for a CPU
    assert!(worked > Duration::from_millis(30), "{:?}", worked);
  }
}
