mod capture;
mod cli;
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

/// Program entrypoint.
fn main() -> Result<()> {
    env_logger::init();
    let args = Args::parse_args();

    if args.edit {
        let target = match args.edit_target() {
            Some(path) => path,
            // `--edit` with no explicit path: edit the most recently taken
            // screenshot instead of falling through to a brand new capture.
            None => output::latest_screenshot_path()?,
        };
        return editor::run(&target);
    }

    if args.record {
        return record::run(&args);
    }

    session::run(args)
}
