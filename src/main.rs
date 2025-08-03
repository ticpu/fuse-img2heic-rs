use anyhow::Result;
use clap::{Parser, Subcommand};
use log::info;
use std::path::PathBuf;

mod cache;
mod config;
mod file_detector;
mod filesystem;
mod image_converter;
mod mount_management;
mod thread_pool;

use crate::config::Config;
use crate::filesystem::ImageFuseFS;

#[derive(Parser)]
#[command(name = "fuse-img2heic")]
#[command(about = "FUSE filesystem that converts images to HEIC format on-the-fly")]
struct Args {
    #[command(subcommand)]
    command: Option<Commands>,

    #[arg(
        short,
        long,
        help = "Mount point override (overrides config file setting)"
    )]
    mount: Option<PathBuf>,

    #[arg(
        short,
        long,
        help = "Path to configuration file (default: ~/.config/fuse-img2heic-rs/config.yaml)"
    )]
    config: Option<PathBuf>,

    #[arg(short, long, help = "Run in foreground mode")]
    foreground: bool,

    #[arg(short, long, action = clap::ArgAction::Count, help = "Verbose logging (-v = INFO, -vv = DEBUG, -vvv = TRACE)")]
    verbose: u8,
}

#[derive(Subcommand)]
enum Commands {
    /// Create configuration directories and default config file
    Setup,
}

fn setup() -> Result<()> {
    println!("Setting up fuse-img2heic-rs...");

    // Create config directory
    let config_path = Config::get_default_config_path()?;
    if let Some(config_dir) = config_path.parent() {
        std::fs::create_dir_all(config_dir)?;
        println!("✓ Created config directory: {}", config_dir.display());
    }

    // Create cache directory
    let cache_dir = Config::get_cache_dir()?;
    println!("✓ Created cache directory: {}", cache_dir.display());

    // Create default config if it doesn't exist
    if !config_path.exists() {
        let config = Config::default();
        config.save(&config_path)?;
        println!("✓ Created default config: {}", config_path.display());
    } else {
        println!("• Config already exists: {}", config_path.display());
    }

    println!("\nSetup complete! You can now:");
    println!("1. Edit the config file: {}", config_path.display());
    println!("2. Run: fuse-img2heic");
    println!("   Or override mount point: fuse-img2heic -m /mnt/heic-images");

    Ok(())
}

fn main() -> Result<()> {
    let args = Args::parse();

    // Set up logging based on verbosity
    let log_level = match args.verbose {
        0 => "warn",  // Default: warnings only
        1 => "info",  // -v: info and above
        2 => "debug", // -vv: debug and above (shows conversion activity)
        _ => "trace", // -vvv+: trace everything
    };

    let fuser_level = if args.verbose >= 3 {
        log::LevelFilter::Debug
    } else {
        log::LevelFilter::Off
    };
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(log_level))
        .filter_module("fuser", fuser_level) // Only show fuser logs at -vvv
        .init();

    // Handle subcommands
    match args.command {
        Some(Commands::Setup) => return setup(),
        None => {}
    }

    let config_path = match args.config {
        Some(path) => path,
        None => Config::get_default_config_path()?,
    };

    info!("Loading configuration from: {config_path:?}");
    let config = Config::load(&config_path)?;

    // Use mount point from CLI arg or config file
    let mount_point = args.mount.unwrap_or(config.mount_point.clone());

    // Ensure mount point exists and is accessible
    mount_management::ensure_mount_point_accessible(&mount_point)?;

    info!("Initializing FUSE filesystem");
    let fs = ImageFuseFS::new(&config, mount_point.clone())?;

    // Set up signal handling for graceful shutdown
    mount_management::setup_shutdown_handler(mount_point.clone())?;

    info!("Mounting filesystem at: {mount_point:?}");
    let options = vec![
        fuser::MountOption::FSName("fuse-img2heic".to_string()),
        fuser::MountOption::AllowOther,
        fuser::MountOption::DefaultPermissions,
    ];

    if args.foreground {
        info!("Running in foreground mode");
        if let Err(e) = fuser::mount2(fs, &mount_point, &options) {
            return Err(anyhow::anyhow!("Failed to mount filesystem: {e}"));
        }
    } else {
        info!("Running in background mode");
        if let Err(e) = fuser::spawn_mount2(fs, &mount_point, &options) {
            return Err(anyhow::anyhow!("Failed to spawn mount filesystem: {e}"));
        }

        // Keep the main thread alive
        loop {
            std::thread::sleep(std::time::Duration::from_secs(60));
        }
    }

    Ok(())
}
