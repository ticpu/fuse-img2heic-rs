use anyhow::Result;
use clap::{Parser, Subcommand};
use fuse3::raw::Session;
use fuse3::MountOptions;
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

    let config_path = Config::get_default_config_path()?;
    if let Some(config_dir) = config_path.parent() {
        std::fs::create_dir_all(config_dir)?;
        println!("Created config directory: {}", config_dir.display());
    }

    let cache_dir = Config::get_cache_dir()?;
    println!("Created cache directory: {}", cache_dir.display());

    if !config_path.exists() {
        let config = Config::default();
        config.save(&config_path)?;
        println!("Created default config: {}", config_path.display());
    } else {
        println!("Config already exists: {}", config_path.display());
    }

    println!("\nSetup complete! You can now:");
    println!("1. Edit the config file: {}", config_path.display());
    println!("2. Run: fuse-img2heic");
    println!("   Or override mount point: fuse-img2heic -m /mnt/heic-images");

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let log_level = match args.verbose {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };

    let fuse3_level = if args.verbose >= 3 {
        log::LevelFilter::Debug
    } else {
        log::LevelFilter::Off
    };
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(log_level))
        .filter_module("fuse3", fuse3_level)
        .init();

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

    let mount_point = args.mount.unwrap_or(config.mount_point.clone());

    mount_management::ensure_mount_point_accessible(&mount_point)?;

    info!("Initializing FUSE filesystem");
    let fs = ImageFuseFS::new(&config, mount_point.clone())?;

    let mut mount_options = MountOptions::default();
    mount_options
        .fs_name("fuse-img2heic")
        .allow_other(true)
        .default_permissions(true)
        .read_only(true);

    info!("Mounting filesystem at: {mount_point:?}");

    let mount_handle = Session::new(mount_options)
        .mount_with_unprivileged(fs, &mount_point)
        .await?;

    info!("Filesystem mounted successfully");

    tokio::signal::ctrl_c().await?;
    info!("Received shutdown signal, unmounting...");

    mount_handle.unmount().await?;
    info!("Filesystem unmounted");

    Ok(())
}
