extern crate target_info;
use clap::{App, AppSettings, Arg, ArgMatches};
use env_logger::Env;
use mothra::{cli_app, gossip, Mothra, NetworkMessage, Subscriber, TaskExecutor};
use slog::{debug, info, o, trace, warn, Drain, Level, Logger};
use std::{thread, time};
use tokio::runtime::Runtime;
use tokio::{signal, sync::mpsc, task};

struct Client;

impl Client {
    pub fn new() -> Self {
        Client {}
    }
}

impl Subscriber for Client {
    fn init(&mut self, network_send: mpsc::UnboundedSender<NetworkMessage>, fork_id: Vec<u8>) {}

    fn discovered_peer(&self, peer: String) {
        println!("Rust: discovered peer");
        println!("peer={:?}", peer);
    }

    fn receive_gossip(&self, message_id: String, sequence_number: u64, agent_string: String, peer_id: String, topic: String, data: Vec<u8>) {
        println!("Rust: received gossip");
        println!("message id={:?}", message_id);
        println!("peer id={:?}", peer_id);
        println!("topic={:?}", topic);
        println!("data={:?}", String::from_utf8_lossy(&data));
    }

    fn receive_rpc(&self, method: String, req_resp: u8, peer: String, data: Vec<u8>) {
        println!("Rust: received rpc");
        println!("method={:?}", method);
        println!("req_resp={:?}", req_resp);
        println!("peer={:?}", peer);
        println!("data={:?}", String::from_utf8_lossy(&data));
    }
}

fn main() {
    let start = time::Instant::now();
    // Parse the CLI parameters.
    let matches = App::new("rust-example")
        .version(clap::crate_version!())
        .author("Jonny Rhea")
        .about("Mothra example app")
        .arg(
            Arg::with_name("foo")
                .long("foo")
                .short("f")
                .value_name("FOO")
                .help("This is a dummy option.")
                .takes_value(false),
        )
        .subcommand(cli_app())
        .get_matches();

    if matches.is_present("foo") {
        println!("Foo flag found");
    }

    let config = Mothra::get_config(
        Some("rust-example".into()),
        Some(format!("v{}-unstable", env!("CARGO_PKG_VERSION"))),
        Some("rust-example/libp2p".into()),
        &matches.subcommand_matches("mothra").unwrap(),
    );
    // configure logging
    env_logger::Builder::from_env(Env::default()).init();
    let decorator = slog_term::TermDecorator::new().build();
    let drain = slog_term::CompactFormat::new(decorator).build().fuse();
    let drain = slog_async::Async::new(drain).build();
    let drain = match config.debug_level.as_str() {
        "info" => drain.filter_level(Level::Info),
        "debug" => drain.filter_level(Level::Debug),
        "trace" => drain.filter_level(Level::Trace),
        "warn" => drain.filter_level(Level::Warning),
        "error" => drain.filter_level(Level::Error),
        "crit" => drain.filter_level(Level::Critical),
        _ => drain.filter_level(Level::Info),
    };
    let slog = Logger::root(drain.fuse(), o!());
    let log = slog.new(o!("Rust-Example" => "Rust-Example"));
    let enr_fork_id = [0u8; 32].to_vec();
    let meta_data = [0u8; 32].to_vec();
    let ping_data = [0u8; 32].to_vec();
    let client = Box::new(Client::new()) as Box<dyn Subscriber + Send>;
    let mut runtime = Runtime::new()
        .map_err(|e| format!("Failed to start runtime: {:?}", e))
        .unwrap();
    let (network_exit_signal, exit) = exit_future::signal();
    let task_executor = TaskExecutor::new(
        runtime.handle().clone(),
        exit,
        log.new(o!("Rust-Example" => "TaskExecutor")),
    );
    let mothra_log = log.new(o!("Rust-Example" => "Mothra"));
    runtime.block_on(async move {
        task::spawn_blocking(move || {
            let (network_globals, network_send) = Mothra::new(
                config,
                enr_fork_id,
                meta_data,
                ping_data,
                &task_executor,
                client,
                mothra_log.clone(),
            )
            .unwrap();
            let dur = time::Duration::from_secs(5);
            loop {
                thread::sleep(dur);
                let topic = "/mothra/topic1".to_string();
                let data = format!("Hello from Rust.  Elapsed time: {:?}", start.elapsed())
                    .as_bytes()
                    .to_vec();
                gossip(network_send.clone(), topic, data, mothra_log.clone());
            }
        });
        // block the current thread until SIGINT is received.
        signal::ctrl_c().await.expect("failed to listen for event");
    });

    warn!(log, "Sending shutdown signal.");
    let _ = network_exit_signal.fire();
    runtime.shutdown_timeout(tokio::time::Duration::from_millis(300));
}
