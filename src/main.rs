mod ai;
mod config;
mod gpui_workspace;

use std::path::PathBuf;

use re_platform::instance::{self, InstanceRole};

fn main() {
    // Absolute, so the running instance (with its own working directory)
    // opens the right files.
    let initial_paths: Vec<PathBuf> = std::env::args_os()
        .skip(1)
        .map(PathBuf::from)
        .map(|path| std::path::absolute(&path).unwrap_or(path))
        .collect();
    let instance_server = match instance::acquire(&initial_paths) {
        Ok(InstanceRole::Forwarded) => return,
        Ok(InstanceRole::Primary(server)) => Some(server),
        Err(error) => {
            eprintln!("RustEditor single-instance coordination failed: {error}");
            None
        }
    };
    gpui_workspace::run(initial_paths, instance_server);
}
