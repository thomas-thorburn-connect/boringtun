// Copyright (c) 2019 Cloudflare, Inc. All rights reserved.
// SPDX-License-Identifier: BSD-3-Clause

use boringtun::device::drop_privileges::drop_privileges;
use boringtun::device::{DeviceConfig, DeviceHandle};
use clap::{Arg, Command};
use daemonize::{Daemonize, Outcome};
use std::fs::OpenOptions;
use std::os::unix::net::UnixDatagram;
use std::process::exit;
use tracing::Level;

fn check_tun_name(_v: String) -> Result<(), String> {
    #[cfg(any(target_os = "macos", target_os = "ios", target_os = "tvos"))]
    {
        if boringtun::device::tun::parse_utun_name(&_v).is_ok() {
            Ok(())
        } else {
            Err("Tunnel name must have the format 'utun[0-9]+', use 'utun' for automatic assignment".to_owned())
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        Ok(())
    }
}

fn main() {
    let matches = Command::new("boringtun")
        .version(env!("CARGO_PKG_VERSION"))
        .author("Vlad Krasnov <vlad@cloudflare.com>")
        .args(&[
            Arg::new("INTERFACE_NAME")
                .required(true)
                .takes_value(true)
                .validator(|tunname| check_tun_name(tunname.to_string()))
                .help("The name of the created interface"),
            Arg::new("foreground")
                .long("foreground")
                .short('f')
                .help("Run and log in the foreground"),
            Arg::new("threads")
                .takes_value(true)
                .long("threads")
                .short('t')
                .env("WG_THREADS")
                .help("Number of OS threads to use")
                .default_value("4"),
            Arg::new("verbosity")
                .takes_value(true)
                .long("verbosity")
                .short('v')
                .env("WG_LOG_LEVEL")
                .possible_values(["error", "info", "debug", "trace"])
                .help("Log verbosity")
                .default_value("error"),
            Arg::new("uapi-fd")
                .long("uapi-fd")
                .env("WG_UAPI_FD")
                .help("File descriptor for the user API")
                .default_value("-1"),
            Arg::new("pid-file")
                .takes_value(true)
                .long("pid-file")
                .env("WG_PID_FILE")
                .help("Write the process ID to a file"),
            Arg::new("work-dir")
                .takes_value(true)
                .long("work-dir")
                .env("WG_WORK_DIR")
                .help("Working directory, default is /tmp")
                .default_value("/tmp"),
            Arg::new("tun-fd")
                .long("tun-fd")
                .env("WG_TUN_FD")
                .help("File descriptor for an already-existing TUN device")
                .default_value("-1"),
            Arg::new("log")
                .takes_value(true)
                .long("log")
                .short('l')
                .env("WG_LOG_FILE")
                .help("Log file")
                .default_value("/tmp/boringtun.out"),
            Arg::new("disable-drop-privileges")
                .long("disable-drop-privileges")
                .env("WG_SUDO")
                .help("Do not drop sudo privileges"),
            Arg::new("disable-connected-udp")
                .long("disable-connected-udp")
                .help("Disable connected UDP sockets to each peer"),
            #[cfg(target_os = "linux")]
            Arg::new("disable-multi-queue")
                .long("disable-multi-queue")
                .help("Disable using multiple queues for the tunnel interface"),
        ])
        .get_matches();

    let background = !matches.is_present("foreground");
    #[cfg(target_os = "linux")]
    let uapi_fd: i32 = matches.value_of_t("uapi-fd").unwrap_or_else(|e| e.exit());
    let tun_fd: isize = matches.value_of_t("tun-fd").unwrap_or_else(|e| e.exit());
    let mut tun_name = matches.value_of("INTERFACE_NAME").unwrap();
    if tun_fd >= 0 {
        tun_name = matches.value_of("tun-fd").unwrap();
    }
    let n_threads: usize = matches.value_of_t("threads").unwrap_or_else(|e| e.exit());
    let log_level: Level = matches.value_of_t("verbosity").unwrap_or_else(|e| e.exit());

    // Create a socketpair to communicate between forked processes
    let (sock1, sock2) = UnixDatagram::pair().unwrap();
    let _ = sock1.set_nonblocking(true);

    let log = matches.value_of("log").unwrap();

    if background {
        let mut daemonize =
            Daemonize::new().working_directory(matches.value_of("work-dir").unwrap());

        if let Some(pid_file) = matches.value_of("pid-file") {
            daemonize = daemonize.pid_file(pid_file);
        }

        let outcome = daemonize.execute();

        match outcome {
            Outcome::Parent(Ok(parent))=> {

                let mut b = [0u8; 1];
                if sock2.recv(&mut b).is_ok() && b[0] == 1 {
                    eprintln!("BoringTun started successfully");
                    exit(parent.first_child_exit_code);
                } else {
                    eprintln!("BoringTun failed to start");
                    exit(1);
                };
            }
            Outcome::Child(Ok(_)) => {
                let log_file = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(log)
                    .unwrap_or_else(move |_| panic!("Could not create log file {}", &log));

                let (non_blocking, _guard) = tracing_appender::non_blocking(log_file);

                tracing_subscriber::fmt()
                    .with_thread_ids(true)
                    .with_max_level(log_level)
                    .with_writer(non_blocking)
                    .with_ansi(false)
                    .init();

                tracing::info!(message = "Started BoringTun child process");
            }
            Outcome::Parent(Err(e)) | Outcome::Child(Err(e)) => {
                eprintln!("BoringTun failed to daemonize: {}", e);
                exit(1);
            }
        };

    } else {
        tracing_subscriber::fmt()
            .pretty()
            .with_max_level(log_level)
            .init();
    }

    let config = DeviceConfig {
        n_threads,
        #[cfg(target_os = "linux")]
        uapi_fd,
        use_connected_socket: !matches.is_present("disable-connected-udp"),
        #[cfg(target_os = "linux")]
        use_multi_queue: !matches.is_present("disable-multi-queue"),
    };

    let mut device_handle: DeviceHandle = match DeviceHandle::new(tun_name, config) {
        Ok(d) => d,
        Err(e) => {
            // Notify parent that tunnel initialization failed
            tracing::error!(message = "Failed to initialize tunnel", error=?e);
            sock1.send(&[0]).unwrap();
            exit(1);
        }
    };

    if !matches.is_present("disable-drop-privileges") {
        if let Err(e) = drop_privileges() {
            tracing::error!(message = "Failed to drop privileges", error = ?e);
            sock1.send(&[0]).unwrap();
            exit(1);
        }
    }

    // Notify parent that tunnel initialization succeeded
    sock1.send(&[1]).unwrap();
    drop(sock1);

    device_handle.wait();
}
