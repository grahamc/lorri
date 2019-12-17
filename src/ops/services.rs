//! Control development services.

use crate::build_loop::{BuildLoop, Event};
use crate::ops::error::{ok, OpResult};
use crate::project::Project;
use crate::thread::Pool;
use crossbeam_channel as chan;
use futures::channel::oneshot;
use futures::future::{self, Either};
use futures::prelude::*;
use slog_scope::{debug, error, info, warn};
use std::collections::HashMap;
use std::fmt::Debug;
use std::fs::File;
use std::path::PathBuf;
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

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Hash)]
struct Service {
    name: String,
    program: PathBuf,
    args: Vec<String>,
}

/// See the documentation for lorri::cli::Command::Services.
pub fn main(services_nix: PathBuf) -> OpResult {
    let (tx, rx) = chan::unbounded();
    let nix_file = crate::NixFile::from(
        services_nix
            .canonicalize()
            .expect("failed to canonicalize services nix file path"),
    );
    let (mut service_manager_tx, service_manager_rx) = channel(1024);

    let mut pool = Pool::new();
    pool.spawn("build-loop", move || {
        let paths = &crate::ops::get_paths().unwrap();
        let project =
            Project::new(nix_file, &paths.gc_root_dir(), paths.cas_store().clone()).unwrap();

        let mut build_loop = BuildLoop::new(&project);

        // The `watch` command does not currently react to pings, hence the `chan::never()`
        build_loop.forever(tx, chan::never());
    })
    .unwrap();
    pool.spawn("logger", move || {
        rx.iter()
            .inspect(|msg| {
                info!("build msg: {:?}", msg);
            })
            .filter(|msg| match msg {
                Event::Completed(_) => true,
                _ => false,
            })
            .inspect(|_msg| {
                info!("starting a new build for the services nix file");
            })
            .map(|_| {
                // start a build on the services file to get the services.json document
                // !!! note: this re-evaluation isn't acceptable for release (ie: big
                // projects don't want to evaluate 3x per change!)
                crate::nix::CallOpts::file(&services_nix).path()
            })
            .for_each(|result| {
                Runtime::new()
                    .unwrap()
                    .block_on(service_manager_tx.send(result.unwrap()))
                    .unwrap()
            });
    })
    .unwrap();

    pool.spawn("service-manager-async", move || {
        Runtime::new()
            .unwrap()
            .block_on(main_async(service_manager_rx));
    })
    .unwrap();
    pool.join_all_or_panic();

    ok()
}

async fn main_async(file_rx: Receiver<(crate::nix::StorePath, crate::nix::GcRootTempDir)>) {
    file_rx
        .filter_map::<_, (Services, crate::nix::GcRootTempDir), _>(|(store_path, gc_root)| async move {
            // Convert a given message in to Services
            let store_path = store_path.as_path();

            let handle = match File::open(store_path) {
                Ok(f) => f,
                Err(e) => {
                    error!("failed to open services definition '{}' for reading", store_path.display(); "error" => ?e);
                    return None;
                }
            };

            match serde_json::from_reader(std::io::BufReader::new(handle)) {
                Ok(services) => Some((services, gc_root)),
                Err(e) => {
                    error!("failed to parse '{}' as a list of services", store_path.display(); "error" => ?e);
                    return None;
                }
            }
        })
        .fold::<ProcessGroup, _, _>(ProcessGroup::empty(), |mut previous_process_group, (mut services, _gc_root)| async move {
            enum ServiceProc {
                AlreadyRunning(ProcessGroupMember),
                ToStart(Service)
            }

            let procs: Vec<_> = services.services
                .drain(..)
                .map(|service| {
                    match previous_process_group.take(&service) {
                        Some(proc) => {
                            info!("Adopting {:?} from prior group", &service);
                            ServiceProc::AlreadyRunning(proc)
                        },
                        None => {
                            info!("Spawning a new {:?}", &service);
                            ServiceProc::ToStart(service)
                        }
                    }
                })
                .collect();

            drop(previous_process_group);

            let mut new_group = ProcessGroup::empty();

            for proc in procs {
                match proc {
                    ServiceProc::AlreadyRunning(proc) => new_group.adopt(proc),
                    ServiceProc::ToStart(service) => {
                        let (stop, stopped) = oneshot::channel::<()>();

                        new_group.insert(service.clone(), stop);
                        tokio::spawn(start_service(service, stopped));
                    }
                }
            }

            new_group
        })
        .await;
}

async fn start_service(service: Service, stop: oneshot::Receiver<()>) {
    info!("starting"; "name" => &service.name);
    let mut child = Command::new(&service.program)
        .args(&service.args)
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
            info!("terminating service"; "name" => &name);
            child.kill().unwrap()
        }
        Either::Right((status, _)) => {
            let status = status.unwrap();
            info!("service exited"; "name" => &name);
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

#[derive(Default, Debug)]
struct ProcessGroup {
    processes: HashMap<Service, oneshot::Sender<()>>,
}

impl ProcessGroup {
    fn empty() -> Self {
        ProcessGroup {
            processes: HashMap::new(),
        }
    }

    fn insert(&mut self, service: Service, kill: oneshot::Sender<()>) {
        self.processes.insert(service, kill);
    }

    fn take(&mut self, service: &Service) -> Option<ProcessGroupMember> {
        let (service, stop) = self.processes.remove_entry(&service)?;
        Some(ProcessGroupMember(service, stop))
    }

    fn adopt(&mut self, member: ProcessGroupMember) {
        self.processes.insert(member.0, member.1);
    }
}

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        self.processes.drain().for_each(|(service, stop)| {
            if let Err(_) = stop.send(()) {
                debug!(
                    "Failed to send stop message to service: {:#?} (it probably died.)",
                    &service
                );
            }
        });
    }
}

struct ProcessGroupMember(Service, oneshot::Sender<()>);
