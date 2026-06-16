use anyhow::{Context, Result};
use loomem_core::Config;
use std::path::Path;
use tracing::{info, warn};

pub async fn check_resources(config: &Config) -> Result<()> {
    info!("Running resource guards...");

    check_disk_space(
        &config.storage.data_dir,
        config.resource_guards.min_disk_space_mb,
    )?;
    log_system_info();
    validate_config_limits(config)?;

    info!("Resource guards passed");
    Ok(())
}

fn check_disk_space(path: &Path, min_mb: u64) -> Result<()> {
    // Create directory if it doesn't exist for the check
    std::fs::create_dir_all(path)
        .with_context(|| format!("Failed to create directory: {:?}", path))?;

    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;

        // Use statvfs syscall via libc to get available disk space
        let path_cstring = std::ffi::CString::new(path.as_os_str().as_bytes())
            .with_context(|| format!("Invalid path for disk check: {:?}", path))?;

        // SAFETY: statvfs is called with a valid null-terminated path and a properly
        // allocated statvfs struct. The pointer is valid for the duration of the call.
        let available_mb = unsafe {
            let mut stat: libc::statvfs = std::mem::zeroed();
            if libc::statvfs(path_cstring.as_ptr(), &mut stat) != 0 {
                let err = std::io::Error::last_os_error();
                return Err(anyhow::anyhow!("statvfs failed for {:?}: {}", path, err));
            }
            // f_bavail: free blocks available to unprivileged processes
            // f_frsize: fundamental file system block size
            #[allow(clippy::unnecessary_cast)]
            // platform-specific: f_bavail is u32 on macOS/some BSD libc, u64 on Linux glibc; cast preserves cross-platform compat
            let available_bytes = stat.f_bavail as u64 * stat.f_frsize;
            available_bytes / (1024 * 1024)
        };

        info!("Data directory: {:?}", path);
        info!(
            "Available disk space: {} MB, minimum required: {} MB",
            available_mb, min_mb
        );

        if available_mb < min_mb {
            tracing::error!(
                "Insufficient disk space: {} MB available, {} MB required at {:?}",
                available_mb,
                min_mb,
                path
            );
            return Err(anyhow::anyhow!(
                "Insufficient disk space: {} MB available, {} MB required",
                available_mb,
                min_mb
            ));
        }
    }

    #[cfg(not(unix))]
    {
        info!("Data directory: {:?}", path);
        info!("Minimum required disk space: {} MB", min_mb);
        warn!("Disk space checking not implemented for non-Unix platforms");
    }

    Ok(())
}

fn log_system_info() {
    info!("System information:");
    info!("  CPU cores: {}", num_cpus::get());

    #[cfg(target_os = "macos")]
    {
        if let Ok(output) = std::process::Command::new("sysctl")
            .args(["-n", "hw.memsize"])
            .output()
        {
            if let Ok(memsize) = String::from_utf8(output.stdout) {
                if let Ok(bytes) = memsize.trim().parse::<u64>() {
                    let mb = bytes / 1024 / 1024;
                    info!("  Total memory: {} MB", mb);
                }
            }
        }
    }

    #[cfg(target_os = "linux")]
    {
        if let Ok(contents) = std::fs::read_to_string("/proc/meminfo") {
            for line in contents.lines() {
                if line.starts_with("MemTotal:") {
                    if let Some(kb) = line.split_whitespace().nth(1) {
                        if let Ok(kb_val) = kb.parse::<u64>() {
                            let mb = kb_val / 1024;
                            info!("  Total memory: {} MB", mb);
                        }
                    }
                    break;
                }
            }
        }
    }
}

fn validate_config_limits(config: &Config) -> Result<()> {
    // Validate that resource limits are reasonable
    let available_cpus = num_cpus::get() as f64;

    if config.resource_guards.max_cpu_cores > available_cpus {
        warn!(
            "Configured max_cpu_cores ({}) exceeds available CPUs ({})",
            config.resource_guards.max_cpu_cores, available_cpus
        );
    }

    if config.resource_guards.max_memory_mb < 64 {
        warn!(
            "Configured max_memory_mb is very low: {} MB",
            config.resource_guards.max_memory_mb
        );
    }

    if config.resource_guards.max_memory_mb > 1024 {
        info!(
            "Configured max_memory_mb is high: {} MB",
            config.resource_guards.max_memory_mb
        );
    }

    Ok(())
}
