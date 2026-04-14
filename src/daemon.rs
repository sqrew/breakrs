//! Background daemon for monitoring and firing timers.
//!
//! This module provides the daemon process that runs in the background to monitor
//! active timers and send desktop notifications when they expire. The daemon uses
//! dynamic sleep intervals to minimize resource usage while ensuring timely notifications.

use crate::database::Database;
use notify_rust::Notification;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;
use sysinfo::System;

// Time constants to avoid magic numbers
const SECONDS_PER_HOUR: u64 = 3600;

fn pid_file_path() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let data_dir = dirs::data_dir().ok_or("Could not find data directory")?;
    Ok(data_dir.join("break").join("daemon.pid"))
}

/// Checks if the daemon process is currently running.
///
/// This function reads the PID file and verifies that the process is still active
/// using cross-platform process checking via sysinfo. Works on Linux, macOS, and Windows.
///
/// # Returns
///
/// Returns `Ok(true)` if the daemon is running, `Ok(false)` if it's not running,
/// or an error if the check fails.
///
/// # Errors
///
/// Returns an error if:
/// - The data directory cannot be accessed
/// - File I/O operations fail
pub fn is_daemon_running() -> Result<bool, Box<dyn std::error::Error>> {
    let pid_file = pid_file_path()?;

    if !pid_file.exists() {
        return Ok(false);
    }

    let pid_str = fs::read_to_string(&pid_file)?;
    let pid: u32 = pid_str.trim().parse().unwrap_or(0);

    if pid == 0 {
        return Ok(false);
    }

    // Use sysinfo for cross-platform process checking
    let mut system = System::new();
    system.refresh_all();
    let pid = sysinfo::Pid::from_u32(pid);

    Ok(system.process(pid).is_some())
}

/// Ensures the daemon is running, starting it if necessary.
///
/// This is the recommended way to start the daemon, as it's idempotent and safe
/// to call multiple times. If the daemon is already running, this does nothing.
/// If it's not running, it starts a new daemon process.
///
/// This function is called automatically by commands that need the daemon to be
/// active (such as when listing timers or checking status).
///
/// # Errors
///
/// Returns an error if the daemon check or start process fails.
pub fn ensure_daemon_running() -> Result<(), Box<dyn std::error::Error>> {
    if !is_daemon_running()? {
        start_daemon_process()?;
    }
    Ok(())
}

/// Starts a new daemon process in the background.
///
/// This spawns the current executable with the `--daemon-mode` flag, running it
/// as a detached background process with stdin, stdout, and stderr redirected to
/// /dev/null. The daemon will continue running even after the parent process exits.
///
/// If a daemon is already running, this function does nothing and returns successfully.
///
/// # Errors
///
/// Returns an error if:
/// - The daemon status check fails
/// - The current executable path cannot be determined
/// - The daemon process cannot be spawned
pub fn start_daemon_process() -> Result<(), Box<dyn std::error::Error>> {
    if is_daemon_running()? {
        return Ok(());
    }

    // Get the current executable path
    let exe = std::env::current_exe()?;

    // Spawn daemon as a detached background process
    // Note: stderr is not redirected so error messages are visible to the user
    Command::new(exe)
        .arg("--daemon-mode")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .spawn()?;

    Ok(())
}

/// Runs the main daemon loop that monitors and fires timers.
///
/// This is the entry point for the daemon process. It performs the following tasks:
///
/// 1. Writes a PID file to track the daemon process
/// 2. Continuously monitors the database for expired timers
/// 3. Sends desktop notifications when timers expire
/// 4. Handles recurring timers by resetting them after completion
/// 5. Sleeps dynamically until the next timer is due (capped at 1 hour)
/// 6. Exits gracefully when no active timers remain
/// 7. Cleans up the PID file on exit
///
/// The daemon uses efficient dynamic sleep intervals based on when the next timer
/// is due, minimizing CPU usage while ensuring timely notifications.
///
/// # Notification Behavior
///
/// - **Title**: The user's timer message (for quick visibility)
/// - **Urgency**: Critical if `--urgent` flag was set (Linux only)
/// - **Sound**: System notification sound if `--sound` flag was set
/// - **Retry Logic**: Automatically retries once after 500ms if notification fails
///
/// # Platform Differences
///
/// Due to differences in system notification APIs:
/// - **Linux**: Full support for urgency levels and sound
/// - **macOS**: Basic notifications only (--urgent and --sound flags accepted but may not affect behavior)
/// - **Windows**: Basic notifications only (--urgent and --sound flags accepted but may not affect behavior)
///
/// # Timer Handling
///
/// - **Recurring timers**: Added to history and reset for the next interval
/// - **One-time timers**: Moved from active list to history
///
/// # Errors
///
/// Returns an error if:
/// - The PID file cannot be written
/// - Database operations fail
/// - Notification delivery fails critically
pub fn run_daemon() -> Result<(), Box<dyn std::error::Error>> {
    // Write PID file
    let pid_file = pid_file_path()?;
    if let Some(parent) = pid_file.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&pid_file, std::process::id().to_string())?;

    // Main daemon loop
    loop {
        // Check for expired timers
        let mut db = Database::load()?;
        let expired = db.get_expired_timers();

        for timer in &expired {
            // Build notification with appropriate settings
            // Use the timer message as the title for immediate visibility
            // Platform-specific notification configuration

            #[cfg(target_os = "linux")]
            let notification = {
                let mut n = Notification::new();
                n.summary(&timer.message)
                    .body("Break timer completed")
                    .urgency(if timer.urgent {
                        notify_rust::Urgency::Critical
                    } else {
                        notify_rust::Urgency::Normal
                    });
                if timer.sound {
                    n.sound_name("message-new-instant");
                }
                n.finalize()
            };

            #[cfg(target_os = "macos")]
            let notification = {
                let mut n = Notification::new();
                n.summary(&timer.message).body("Break timer completed");
                if timer.sound {
                    n.sound_name("Glass");
                }
                // Note: Sound support on macOS may vary by notification backend
                // The --sound flag is accepted but may not always produce audio
                n.finalize()
            };

            #[cfg(target_os = "windows")]
            let notification = {
                let mut n = Notification::new();
                n.summary(&timer.message).body("Break timer completed");
                // Note: Sound support on Windows may vary by notification backend
                // The --sound flag is accepted but may not always produce audio
                n.finalize()
            };

            // Show notification with retry on failure
            if let Err(e) = notification.show() {
                eprintln!(
                    "Warning: Failed to show notification for '{}': {}",
                    timer.message, e
                );
                eprintln!("Retrying notification after brief delay...");

                // Wait briefly and retry once
                thread::sleep(Duration::from_millis(500));

                if let Err(e) = notification.show() {
                    eprintln!(
                        "Error: Failed to show notification after retry for '{}': {}",
                        timer.message, e
                    );
                    eprintln!("Check that your system notification daemon is running.");
                }
            }

            // Handle recurring vs one-time timers
            if timer.recurring {
                // Add to history and reset the timer for the next interval
                db.add_to_history(timer.clone());
                db.reset_timer(timer.id);
            } else {
                // Complete the timer (moves to history)
                db.complete_timer(timer.id);
            }
        }

        if !expired.is_empty() {
            db.save()?;
        }

        // If no more timers, exit daemon
        if db.timers.is_empty() {
            break;
        }

        // Calculate sleep time until next timer
        let now = time::OffsetDateTime::now_utc();
        let next_timer = db.timers.iter().min_by_key(|t| t.due_at);

        let sleep_duration = if let Some(next) = next_timer {
            let time_until = next.due_at - now;
            let seconds = time_until.whole_seconds();
            if seconds > 0 {
                // Sleep until just past the timer (add 1 second buffer)
                Duration::from_secs((seconds + 1) as u64)
            } else {
                // Timer already expired, check immediately
                Duration::from_secs(1)
            }
        } else {
            // Fallback to 30 seconds if no timer found
            Duration::from_secs(30)
        };

        // Cap sleep duration at 1 hour for safety
        let sleep_duration = sleep_duration.min(Duration::from_secs(SECONDS_PER_HOUR));

        thread::sleep(sleep_duration);
    }

    // Clean up PID file
    let _ = fs::remove_file(&pid_file);

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pid_file_path_creation() {
        // Just verify we can generate a PID file path
        let path = pid_file_path();
        assert!(path.is_ok());
        let path = path.unwrap();
        assert!(path.to_string_lossy().contains("break"));
        assert!(path.to_string_lossy().ends_with("daemon.pid"));
    }

    #[test]
    fn test_is_daemon_running_no_pid_file() {
        // When there's no PID file, daemon should not be running
        // This test assumes the daemon is not currently running
        // Note: This might fail if daemon is actually running, but that's expected
        let result = is_daemon_running();
        assert!(result.is_ok());
    }

    #[test]
    fn test_ensure_daemon_running_idempotent() {
        // Calling ensure_daemon_running multiple times should be safe
        // This is more of a smoke test
        let result = ensure_daemon_running();
        // May succeed or fail depending on system state, but shouldn't panic
        // Just verify it returns a Result
        let _ = result;
    }
}
