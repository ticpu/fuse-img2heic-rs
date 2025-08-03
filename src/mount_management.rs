use anyhow::Result;
use log::{debug, info};
use std::path::Path;

/// Check if a mount point is accessible and attempt to unmount if stuck
pub fn ensure_mount_point_accessible(mount_point: &Path) -> Result<()> {
    debug!("Checking mount point accessibility: {mount_point:?}");

    // First check if we can read the directory - this will catch stuck mounts
    match std::fs::read_dir(mount_point) {
        Ok(_) => {
            debug!("Mount point is accessible");
            Ok(())
        }
        Err(e) => {
            debug!("Failed to read mount point: {e:?}");

            if e.raw_os_error() == Some(107) {
                // Transport endpoint is not connected
                info!("Mount point appears to be stuck from previous mount, attempting to unmount");
                attempt_unmount(mount_point)?;

                // After unmounting, ensure directory exists
                if !mount_point.exists() {
                    info!("Creating mount point after unmount: {mount_point:?}");
                    std::fs::create_dir_all(mount_point)?;
                }
                return Ok(());
            }

            if e.kind() == std::io::ErrorKind::NotFound {
                info!("Creating mount point: {mount_point:?}");
                std::fs::create_dir_all(mount_point)?;
                return Ok(());
            }

            Err(anyhow::anyhow!("Cannot access mount point: {e}"))
        }
    }
}

/// Attempt to unmount a stuck filesystem
fn attempt_unmount(mount_point: &Path) -> Result<()> {
    let mount_str = mount_point
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("Invalid mount point path"))?;

    let output = std::process::Command::new("fusermount")
        .args(["-u", mount_str])
        .output()
        .map_err(|e| anyhow::anyhow!("Failed to run fusermount: {e}"))?;

    if output.status.success() {
        info!("Successfully unmounted stuck filesystem");
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(anyhow::anyhow!("Unmount command failed: {stderr}"))
    }
}

/// Set up graceful shutdown handler
pub fn setup_shutdown_handler(mount_point: std::path::PathBuf) -> Result<()> {
    ctrlc::set_handler(move || {
        info!("Received shutdown signal, unmounting filesystem");
        let _ = attempt_unmount(&mount_point);
        std::process::exit(0);
    })
    .map_err(|e| anyhow::anyhow!("Error setting signal handler: {e}"))
}
