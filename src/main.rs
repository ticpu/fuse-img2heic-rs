use anyhow::Result;
use clap::{Parser, Subcommand};
use log::info;
use std::path::PathBuf;

mod cache;
mod config;
mod file_detector;
mod filesystem;
mod image_converter;
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
    env_logger::init();

    let args = Args::parse();

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

    // Ensure mount point exists
    if !mount_point.exists() {
        info!("Creating mount point: {:?}", mount_point);
        std::fs::create_dir_all(&mount_point)?;
    }

    info!("Initializing FUSE filesystem");
    let fs = ImageFuseFS::new(config)?;

    // Set up signal handling for graceful shutdown
    let mount_point_clone = mount_point.clone();
    ctrlc::set_handler(move || {
        info!("Received shutdown signal, unmounting filesystem");
        // Attempt to unmount the filesystem
        let _ = std::process::Command::new("fusermount")
            .args(["-u", mount_point_clone.to_str().unwrap_or("")])
            .output();
        std::process::exit(0);
    })
    .expect("Error setting signal handler");

    info!("Mounting filesystem at: {:?}", mount_point);
    let options = vec![
        fuser::MountOption::FSName("fuse-img2heic".to_string()),
        fuser::MountOption::AllowOther,
        fuser::MountOption::DefaultPermissions,
    ];

    if args.foreground {
        info!("Running in foreground mode");
        fuser::mount2(fs, &mount_point, &options)?;
    } else {
        info!("Running in background mode");
        fuser::spawn_mount2(fs, &mount_point, &options)?;

        // Keep the main thread alive
        loop {
            std::thread::sleep(std::time::Duration::from_secs(60));
        }
    }

    Ok(())
}
