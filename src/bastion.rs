//!
//!
//! Main module for the runtime.
//!
//! Runtime management code and root supervision happens here.
//! Root supervisor's spawn method is here to have a frictionless start for newcomers.
//!

use crate::child::{BastionChildren, BastionClosure, Message};
use crate::config::BastionConfig;
use crate::context::BastionContext;
use crate::messages::PoisonPill;
use crate::runtime_manager::RuntimeManager;
use crate::runtime_system::RuntimeSystem;
use crate::supervisor::{SupervisionStrategy, Supervisor};
use crate::tramp::Tramp;
use crossbeam_channel::unbounded;
use ego_tree::{NodeRef, Tree};
use env_logger::Builder;
use futures::future::poll_fn;
use lazy_static::lazy_static;
use log::LevelFilter;

use std::mem;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use tokio::prelude::future::FutureResult;
use tokio::prelude::*;
use tokio::runtime::Runtime;
use uuid::Uuid;

lazy_static! {
    /// Platform which contains runtime system.
    pub static ref PLATFORM: Arc<Mutex<RuntimeSystem>> = Arc::new(Mutex::new(RuntimeSystem::start()));

    /// Fault induced supervisors queue
    pub static ref FAULTED: Arc<Mutex<Vec<Supervisor>>> =
        Arc::new(Mutex::new(Vec::<Supervisor>::new()));
}

/// Runtime which holds the runtime configuration and implements methods for
/// configuring root-level supervisors, fault recovery, and set up/tear down methods.
pub struct Bastion {
    /// Initial runtime configuration
    pub config: BastionConfig,
    log_builder: Builder,
}

impl Bastion {
    /// Instantiates the platform from the given configuration.
    ///
    /// # Arguments
    /// * `config` - Platform configuration given for the instantiation
    ///
    /// # Example
    /// ```
    ///# use bastion::prelude::*;
    ///# use log::LevelFilter;
    ///#
    ///# // Instantiate the platform
    ///# fn main() {
    ///let config = BastionConfig {
    ///    log_level: LevelFilter::Debug,
    ///    in_test: false,
    ///};
    ///
    ///Bastion::platform_from_config(config);
    ///# }
    /// ```
    ///
    pub fn platform_from_config(config: BastionConfig) -> Self {
        let log_builder = Builder::from_default_env();

        let mut platform = Bastion {
            config,
            log_builder,
        };

        platform
            .log_builder
            .format(|buf, record| {
                writeln!(buf, "[BASTION] - [{}] - {}", record.level(), record.args())
            })
            .filter(None, platform.config.log_level)
            .is_test(platform.config.in_test)
            .init();

        platform
    }

    /// Instantiates the platform with default configuration.
    ///
    /// # Example
    /// ```
    /// use bastion::prelude::*;
    ///
    /// // Instantiate the platform
    /// fn main() {
    ///    Bastion::platform();
    /// }
    /// ```
    ///
    pub fn platform() -> Self {
        let default_config = BastionConfig {
            log_level: LevelFilter::Info,
            in_test: false,
        };

        Bastion::platform_from_config(default_config)
    }

    ///
    /// Supervisor builder for the runtime.
    ///
    /// # Arguments
    /// * `name` - Supervisor name
    /// * `system` - Supervisor system's name for grouping
    /// multiple supervisor's to the same system.
    ///
    /// After building the supervisor it will be placed under the supervision tree.
    ///
    /// When runtime is started, supervision tree will be traversed back again
    /// to start supervisors and their children.
    ///
    /// Supervisors work exactly like self-contained actor systems.
    /// One addition over ordinary actor system is that,
    /// these supervisors also can communicate with each other with async messages.
    ///
    /// # Example
    ///
    /// ```rust
    ///# use bastion::prelude::*;
    ///# use std::{fs, thread};
    ///#
    ///# fn main() {
    ///#  Bastion::platform();
    ///Bastion::supervisor("background-worker", "fetcher-system")
    ///    .strategy(SupervisionStrategy::OneForAll);
    ///# }
    /// ```
    pub fn supervisor(name: &'static str, system: &'static str) -> Supervisor {
        let sp = Supervisor::default().props(name.into(), system.into());
        Bastion::traverse(sp)
    }

    //
    // Placement projection in the supervision tree
    //
    fn traverse_registry(
        mut registry: Tree<Supervisor>,
        root: NodeRef<Supervisor>,
        new_supervisor: &Supervisor,
    ) {
        for i in root.children() {
            let curn = new_supervisor.urn.clone();
            let k = new_supervisor.clone();
            if i.value().urn == curn {
                let mut ancestor = registry.get_mut(i.id()).unwrap();
                ancestor.append(k);
                break;
            }
            Bastion::traverse_registry(registry.clone(), i, &*new_supervisor)
        }
    }

    fn traverse(ns: Supervisor) -> Supervisor {
        let runtime = PLATFORM.lock().unwrap();
        let arcreg = runtime.registry.clone();
        let registry = arcreg.lock().unwrap();
        Bastion::traverse_registry(registry.clone(), registry.root(), &ns);

        ns
    }

    pub(crate) fn fault_recovery(given: Supervisor, message_box: Box<dyn Message>) {
        // Clone supervisor for trampoline bouncing
        let trampoline_spv = given.clone();

        // Push supervisor for next trampoline
        let fark = FAULTED.clone();
        let mut faulted_ones = fark.lock().unwrap();
        faulted_ones.push(given.clone());

        debug!("Fault induced supervisors: {:?}", faulted_ones);

        let restart_needed = match trampoline_spv.strategy {
            SupervisionStrategy::OneForOne => {
                let killed = trampoline_spv.ctx.killed.clone();
                debug!(
                    "One for One – Children restart triggered for :: {:?}",
                    killed
                );
                killed
            }
            SupervisionStrategy::OneForAll => {
                trampoline_spv.ctx.descendants.iter().for_each(|children| {
                    let tx = children.tx.as_ref().unwrap().clone();
                    debug!(
                        "One for All – Restart triggered for all :: {:?}",
                        children.id
                    );
                    tx.send(PoisonPill::new()).unwrap_or(());
                });

                // Don't make avalanche effect, send messages and wait for all to come back.
                let killed_processes = trampoline_spv.ctx.killed.clone();
                debug!(
                    "One for All – Restart triggered for killed :: {:?}",
                    killed_processes
                );
                killed_processes
            }
            SupervisionStrategy::RestForOne => {
                // Find the rest in the group of killed one.
                trampoline_spv.ctx.killed.iter().for_each(|killed| {
                    let mut rest_to_kill = trampoline_spv.ctx.descendants.clone();
                    rest_to_kill.retain(|i| !killed.id.contains(&i.id));

                    rest_to_kill.iter().for_each(|children| {
                        let tx = children.tx.as_ref().unwrap().clone();
                        debug!(
                            "Rest for One – Restart triggered for rest :: {:?}",
                            children.id
                        );
                        tx.send(PoisonPill::new()).unwrap_or(());
                    });
                });

                let killed_processes = trampoline_spv.ctx.killed.clone();
                debug!(
                    "Rest for One – Restart triggered for killed :: {:?}",
                    killed_processes
                );

                killed_processes
            }
        };

        debug!("Restart Needed for – {:?}", restart_needed);

        Tramp::Traverse(restart_needed).execute(|desc| {
            let message_clone = objekt::clone_box(&*message_box);
            let spv = trampoline_spv.clone();

            match &desc {
                n if n.is_empty() => Tramp::Complete(n.to_vec()),
                n => {
                    let mut rest_child = n.clone();
                    if let Some(children) = rest_child.pop() {
                        //: Box<(dyn BastionClosure<Output = ()> + 'static)> =
                        let bt = objekt::clone_box(&*children.thunk);
                        let message_box = objekt::clone_box(&*message_box);
                        let tx = children.tx.as_ref().unwrap().clone();
                        let rx = children.rx.clone().unwrap();

                        let f = future::lazy(move || {
                            bt(
                                BastionContext {
                                    parent: Some(Box::new(spv.clone())),
                                    descendants: spv.ctx.descendants,
                                    killed: spv.ctx.killed,
                                    bcast_rx: Some(rx.clone()),
                                    bcast_tx: Some(tx.clone()),
                                },
                                message_box,
                            );
                            future::ok::<(), ()>(())
                        });

                        let k = AssertUnwindSafe(f).catch_unwind().then(
                            |result| -> FutureResult<(), ()> {
                                if let Err(err) = result {
                                    error!("Panic happened in restarted - {:?}", err);
                                    let fark = FAULTED.clone();
                                    let mut faulted_ones = fark.lock().unwrap();
                                    let faulted = faulted_ones.pop().unwrap();

                                    // Make trampoline to re-enter
                                    drop(faulted_ones);
                                    drop(fark);
                                    Bastion::fault_recovery(faulted, message_clone);
                                }
                                future::ok(())
                            },
                        );

                        let ark = PLATFORM.clone();
                        let mut runtime = ark.lock().unwrap();
                        let shared_runtime: &mut Runtime = &mut runtime.runtime;
                        shared_runtime.spawn(k);
                    }

                    Tramp::Traverse(rest_child)
                }
            }
        });
    }

    ///
    /// Launcher method to start the runtime system. Runs the system until OS signals a shutdown.
    ///
    /// Platform catches all possible panics caused inside process loops and restart them.
    ///
    /// Note that segmentation faults are not recoverable
    /// and they can happen in any programming language.
    pub fn start() {
        Bastion::runtime_shutdown_callback()
    }

    ///
    /// Forces runtime system to shutdown by forcing the system shutdown.
    ///
    /// Instead of using this it is better to use signals to kill/stop the process.
    ///
    /// **NOTE**: This piece is very unstable and not recommended on production.
    pub fn force_shutdown() {
        Bastion::unstable_shutdown()
    }

    ///
    /// Root supervisor's process spawner.
    ///
    /// If you don't need a supervision strategy other than [SupervisionStrategy::OneForOne], or
    /// if your system doesn't need complex supervision. This is for you.
    ///
    /// This method helps you to get going with the basics.
    ///
    /// # Arguments
    /// * `thunk` - User code which will be executed inside the process.
    /// * `msg` - Initial message which will be passed to the thunk.
    ///
    /// # Example
    /// ```rust
    ///# use bastion::prelude::*;
    ///#
    ///# fn main() {
    ///#    Bastion::platform();
    ///#
    ///#    let message = String::from("Some message to be passed");
    ///#
    /// Bastion::spawn(
    ///     |context: BastionContext, msg: Box<dyn Message>| {
    ///         // Message can be used here.
    ///         receive! { msg,
    ///             String => |o| { println!("Received {}", o) },
    ///             _ => println!("other message type...")
    ///         }
    ///
    ///         println!("root supervisor - spawn_at_root - 1");
    ///
    ///         // Rebind to the system
    ///         context.hook();
    ///     },
    ///     message,
    /// );
    ///
    /// // Comment out to start the system, so runtime can initialize.
    /// // Bastion::start()
    ///# }
    /// ```
    pub fn spawn<F, M>(thunk: F, msg: M) -> BastionChildren
    where
        F: BastionClosure,
        M: Message,
    {
        let bt = Box::new(thunk);
        let msg_box = Box::new(msg);
        let (p, c) = unbounded();
        let child = BastionChildren {
            id: Uuid::new_v4().to_string(),
            tx: Some(p),
            rx: Some(c),
            redundancy: 1_i32,
            msg: objekt::clone_box(&*msg_box),
            thunk: objekt::clone_box(&*bt),
        };

        let message_clone = objekt::clone_box(&*msg_box);
        let if_killed = child.clone();
        let ret_val = child.clone();

        let root_spv;
        {
            let ark = PLATFORM.clone();
            let runtime = ark.lock().unwrap();
            let mut registry = runtime.registry.lock().unwrap();
            let mut rootn = registry.root_mut();
            let root: &mut Supervisor = rootn.value();

            root.ctx.descendants.push(child);

            root_spv = root.clone();
        }

        let tx = ret_val.tx.as_ref().unwrap().clone();
        let rx = ret_val.rx.clone().unwrap();

        let f = future::lazy(move || {
            bt(
                BastionContext {
                    parent: Some(Box::new(root_spv.clone())),
                    descendants: root_spv.ctx.descendants,
                    killed: root_spv.ctx.killed,
                    bcast_rx: Some(rx.clone()),
                    bcast_tx: Some(tx.clone()),
                },
                msg_box,
            );
            future::ok::<(), ()>(())
        });

        let k = AssertUnwindSafe(f)
            .catch_unwind()
            .then(|result| -> FutureResult<(), ()> {
                let ark = PLATFORM.clone();
                let runtime = ark.lock().unwrap();
                let mut registry = runtime.registry.lock().unwrap();
                let mut rootn = registry.root_mut();
                let mut root = rootn.value().clone();

                root.ctx.killed.push(if_killed);

                // Enable re-entrant code
                drop(registry);
                drop(runtime);
                if let Err(err) = result {
                    error!("Panic happened in root level child - {:?}", err);
                    Bastion::fault_recovery(root.clone(), message_clone);
                }
                future::ok(())
            });

        let ark = PLATFORM.clone();
        let mut runtime = ark.lock().unwrap();
        let shared_runtime: &mut Runtime = &mut runtime.runtime;
        shared_runtime.spawn(k);

        ret_val
    }
}

type Never = ();
const CLOSE_OVER: Result<Async<()>, Never> = Ok(Async::Ready(()));

impl RuntimeManager for Bastion {
    fn unstable_shutdown() {
        unsafe {
            if let Ok(lock_ptr) = PLATFORM.clone().try_lock() {
                let l: RuntimeSystem = mem::transmute_copy(&*lock_ptr);
                l.runtime.shutdown_now().wait().unwrap();
                mem::forget(lock_ptr);
            }
        }
    }

    fn runtime_shutdown_callback() {
        let mut entered = tokio_executor::enter().expect("main thread_local runtime lock");
        let running = Arc::new(AtomicBool::new(true));
        let r = running.clone();
        let _ = ctrlc::set_handler(move || {
            r.store(false, Ordering::SeqCst);
        })
        .unwrap();
        entered
            .block_on(poll_fn(|| {
                while running.load(Ordering::SeqCst) {}
                CLOSE_OVER
            }))
            .expect("cannot shutdown");
    }
}
