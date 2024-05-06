use anyhow::Result;
use clap::Parser;

use iroh_net_bench::{
    configure_tracing_subscriber, iroh, quinn, rt, s2n, server, Commands, Endpoint, Opt,
};

fn main() {
    let cmd = Commands::parse();
    configure_tracing_subscriber();

    match cmd {
        Commands::Iroh(opt) => {
            if let Err(e) = run_iroh(opt) {
                eprintln!("failed: {e:#}");
            }
        }
        Commands::Quinn(opt) => {
            if let Err(e) = run_quinn(opt) {
                eprintln!("failed: {e:#}");
            }
        }
        Commands::S2n(opt) => {
            if let Err(e) = run_s2n(opt) {
                eprintln!("failed: {e:#}");
            }
        }
    }
}

pub fn run_iroh(opt: Opt) -> Result<()> {
    let server_span = tracing::error_span!("server");
    let runtime = rt();
    let (server_addr, endpoint) = {
        let _guard = server_span.enter();
        iroh::server_endpoint(&runtime, &opt)
    };

    let server_thread = std::thread::spawn(move || {
        let _guard = server_span.entered();
        if let Err(e) = runtime.block_on(server(Endpoint::Iroh(endpoint), opt)) {
            eprintln!("server failed: {e:#}");
        }
    });

    let mut handles = Vec::new();
    for id in 0..opt.clients {
        let server_addr = server_addr.clone();
        handles.push(std::thread::spawn(move || {
            let _guard = tracing::error_span!("client", id).entered();
            let runtime = rt();
            match runtime.block_on(iroh::client(server_addr, opt)) {
                Ok(stats) => Ok(stats),
                Err(e) => {
                    eprintln!("client failed: {e:#}");
                    Err(e)
                }
            }
        }));
    }

    for (id, handle) in handles.into_iter().enumerate() {
        // We print all stats at the end of the test sequentially to avoid
        // them being garbled due to being printed concurrently
        if let Ok(stats) = handle.join().expect("client thread") {
            stats.print(id);
        }
    }

    server_thread.join().expect("server thread");

    Ok(())
}

pub fn run_quinn(opt: Opt) -> Result<()> {
    let server_span = tracing::error_span!("server");
    let runtime = rt();
    let (server_addr, endpoint) = {
        let _guard = server_span.enter();
        quinn::server_endpoint(&runtime, &opt)
    };

    let server_thread = std::thread::spawn(move || {
        let _guard = server_span.entered();
        if let Err(e) = runtime.block_on(server(Endpoint::Quinn(endpoint), opt)) {
            eprintln!("server failed: {e:#}");
        }
    });

    let mut handles = Vec::new();
    for id in 0..opt.clients {
        handles.push(std::thread::spawn(move || {
            let _guard = tracing::error_span!("client", id).entered();
            let runtime = rt();
            match runtime.block_on(quinn::client(server_addr, opt)) {
                Ok(stats) => Ok(stats),
                Err(e) => {
                    eprintln!("client failed: {e:#}");
                    Err(e)
                }
            }
        }));
    }

    for (id, handle) in handles.into_iter().enumerate() {
        // We print all stats at the end of the test sequentially to avoid
        // them being garbled due to being printed concurrently
        if let Ok(stats) = handle.join().expect("client thread") {
            stats.print(id);
        }
    }

    server_thread.join().expect("server thread");

    Ok(())
}

pub fn run_s2n(_opt: s2n::Opt) -> Result<()> {
    unimplemented!()
}
