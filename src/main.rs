mod capture;
mod cli;
mod config;
mod constants;
mod editor;
mod idle_finish;
mod opencv_compat;
mod output;
mod overlay;
mod record;
mod region_overlay;
mod session;
mod stitch;
mod types;

use anyhow::Result;

use crate::cli::Args;
use crate::config::Config;

/// Program entrypoint.
fn main() -> Result<()> {
    env_logger::init();
    let args = Args::parse_args();
    let config = Config::load();

    if args.edit {
        let target = match args.edit_target() {
            Some(path) => path,
            // `--edit` with no explicit path: edit the most recently taken
            // screenshot instead of falling through to a brand new capture.
            None => output::latest_screenshot_path(config.image_dir.as_deref())?,
        };
        return editor::run(&target);
    }

    if args.record {
        return record::run(&args, &config);
    }

    session::run(args, config)
}
