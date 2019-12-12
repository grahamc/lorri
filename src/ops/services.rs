//! Control development services.

use crate::build_loop::{BuildLoop, Event};
use crate::ops::error::{ok, OpResult};
use crate::project::Project;
use crate::thread::Pool;
use crossbeam_channel as chan;
use futures::channel::oneshot;
use futures::future::{self, Either};
use futures::prelude::*;
use slog_scope::{error, info, warn};
use std::fmt::Debug;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::io::AsyncBufReadExt;
use tokio::io::BufReader;
use tokio::process::{Child, Command};
use tokio::runtime::Runtime;
use tokio::sync::mpsc::{channel, Receiver};

#[derive(Copy, Clone, Debug)]
enum Fd {
    Stdout,
    Stderr,
}

#[derive(Debug)]
struct Log {
    name: String,
    fd: Fd,
    message: String,
}

#[derive(Debug, Deserialize)]
struct Services {
    services: Vec<Service>,
}

#[derive(Debug, Deserialize)]
struct Service {
    name: String,
    program: PathBuf,
    args: Vec<String>,
}

/// See the documentation for lorri::cli::Command::Services.
pub fn main(config: &Path) -> OpResult {
    let (tx, rx) = chan::unbounded();
    let to_build = config.to_path_buf();
    let nixfile = crate::NixFile::from(config.to_path_buf().canonicalize().unwrap());

    let mut threadpool = Pool::new();

    let (mut service_manager_tx, service_manager_rx) = channel(1024);
    threadpool
        .spawn("build-loop", move || {
            let paths = &crate::ops::get_paths().unwrap();
            let project =
                Project::new(nixfile, &paths.gc_root_dir(), paths.cas_store().clone()).unwrap();

            let mut build_loop = BuildLoop::new(&project);

            // The `watch` command does not currently react to pings, hence the `chan::never()`
            build_loop.forever(tx, chan::never());
        })
        .unwrap();
    threadpool
        .spawn("logger", move || {
            rx.iter()
                .inspect(|msg| {
                    info!("build msg: {:?}", msg);
                })
                .filter(|msg| match msg {
                    Event::Completed(_) => true,
                    _ => false,
                })
                .inspect(|_msg| {
                    println!("Starting a new build for the services.nix");
                })
                .map(|_| {
                    // start a build on the services file to get the services.json document
                    // !!! note: this re-evaluation isn't acceptable for release (ie: big
                    // projects don't want to evaluate 3x per change!)
                    crate::nix::CallOpts::file(&to_build).path()
                })
                .for_each(|result| {
                    Runtime::new()
                        .unwrap()
                        .block_on(service_manager_tx.send(result.unwrap()))
                        .unwrap()
                });
        })
        .unwrap();

    threadpool
        .spawn("service-manager-async", move || {
            Runtime::new()
                .unwrap()
                .block_on(main_async(service_manager_rx));
        })
        .unwrap();
    threadpool.join_all_or_panic();

    ok()
}

async fn main_async(mut file_rx: Receiver<(crate::nix::StorePath, crate::nix::GcRootTempDir)>) {
    let mut initial_file = file_rx.recv().await.unwrap();

    loop {
        let services: Services = serde_json::from_reader(std::io::BufReader::new(
            File::open(initial_file.0.as_path()).unwrap(),
        ))
        .unwrap();

        let mut to_kill = vec![];

        for service in services.services {
            let (stop, stopped) = oneshot::channel::<()>();
            to_kill.push(stop);
            tokio::spawn(start_service(service, stopped));
        }

        initial_file = file_rx.recv().await.unwrap();

        for stop in to_kill.into_iter() {
            stop.send(()).unwrap();
        }
    }
}

async fn start_service(service: Service, stop: oneshot::Receiver<()>) {
    info!("starting"; "name" => &service.name);
    let mut child = Command::new(&service.program)
        .args(service.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    tokio::spawn(log_stream(
        BufReader::new(child.stdout().take().unwrap()).lines(),
        service.name.to_string(),
        Fd::Stdout,
    ));
    tokio::spawn(log_stream(
        BufReader::new(child.stderr().take().unwrap()).lines(),
        service.name.to_string(),
        Fd::Stderr,
    ));

    tokio::spawn(cleanup(child, service.name, stop));
}

async fn log_stream<'a, L>(mut lines: L, name: String, fd: Fd)
where
    L: Stream<Item = tokio::io::Result<String>> + std::marker::Unpin,
{
    while let Some(Ok(message)) = lines.next().await {
        match fd {
            Fd::Stdout => info!("{}", message; "name" => &name),
            Fd::Stderr => warn!("{}", message; "name" => &name),
        }
    }
}

async fn cleanup(mut child: Child, name: String, cancel: oneshot::Receiver<()>) {
    let operation = future::select(cancel, &mut child).await;

    match operation {
        Either::Left(_) => {
            info!("Killing child");
            child.kill().unwrap()
        }
        Either::Right((status, _)) => {
            let status = status.unwrap();
            info!("Child exited");
            let code = status
                .code()
                .map_or("<unknown>".to_string(), |c| format!("{}", c));
            if status.success() {
                warn!("service exited"; "name" => name, "code" => code);
            } else {
                error!("service exited"; "name" => name, "code" => code);
            }
        }
    };
}
