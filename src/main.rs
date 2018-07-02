// Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Runs a virtual machine under KVM

extern crate arch;
#[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
extern crate aarch64;
extern crate devices;
extern crate libc;
extern crate io_jail;
extern crate kvm;
extern crate kvm_sys;
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
extern crate x86_64;
extern crate kernel_loader;
extern crate kernel_cmdline;
extern crate byteorder;
extern crate net_util;
extern crate qcow;
#[macro_use]
extern crate sys_util;
extern crate resources;
extern crate vhost;
extern crate vm_control;
extern crate data_model;
#[cfg(feature = "plugin")]
extern crate plugin_proto;
#[cfg(feature = "plugin")]
extern crate protobuf;
#[cfg(feature = "wl-dmabuf")]
extern crate gpu_buffer;

pub mod argument;
pub mod linux;
#[cfg(feature = "plugin")]
pub mod plugin;

use std::net;
use std::os::unix::io::RawFd;
use std::os::unix::net::UnixDatagram;
use std::path::PathBuf;
use std::string::String;
use std::thread::sleep;
use std::time::Duration;

use sys_util::{Scm, getpid, kill_process_group, reap_child, syslog};

use argument::{Argument, set_arguments, print_help};
use vm_control::VmRequest;

static SECCOMP_POLICY_DIR: &'static str = "/usr/share/policy/crosvm";

enum DiskType {
    FlatFile,
    Qcow,
}

struct DiskOption {
    path: PathBuf,
    writable: bool,
    disk_type: DiskType,
}

/// Contains all the info needed to create the system's virtio devices.
/// TODO(dgreid) - remove this once all devices are PCI based instead of MMIO.
pub struct VirtIoDeviceInfo {
    disks: Vec<DiskOption>,
    host_ip: Option<net::Ipv4Addr>,
    netmask: Option<net::Ipv4Addr>,
    mac_address: Option<net_util::MacAddress>,
    vhost_net: bool,
    tap_fd: Option<RawFd>,
    cid: Option<u64>,
    wayland_socket_path: Option<PathBuf>,
    wayland_dmabuf: bool,
    multiprocess: bool,
    seccomp_policy_dir: PathBuf,
}

pub struct Config {
    vcpu_count: Option<u32>,
    memory: Option<usize>,
    kernel_path: PathBuf,
    params: Vec<String>,
    socket_path: Option<PathBuf>,
    plugin: Option<PathBuf>,
    plugin_root: Option<PathBuf>,
    virtio_dev_info: VirtIoDeviceInfo,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            vcpu_count: None,
            memory: None,
            kernel_path: PathBuf::default(),
            params: Vec::new(),
            socket_path: None,
            plugin: None,
            plugin_root: None,
            virtio_dev_info: VirtIoDeviceInfo {
                disks: Vec::new(),
                host_ip: None,
                netmask: None,
                mac_address: None,
                vhost_net: false,
                tap_fd: None,
                cid: None,
                wayland_socket_path: None,
                wayland_dmabuf: false,
                multiprocess: !cfg!(feature = "default-no-sandbox"),
                seccomp_policy_dir: PathBuf::from(SECCOMP_POLICY_DIR),
            },
        }
    }
}

// Wait for all children to exit. Return true if they have all exited, false
// otherwise.
fn wait_all_children() -> bool {
    const CHILD_WAIT_MAX_ITER: isize = 10;
    const CHILD_WAIT_MS: u64 = 10;
    for _ in 0..CHILD_WAIT_MAX_ITER {
        loop {
            match reap_child() {
                Ok(0) => break,
                // We expect ECHILD which indicates that there were no children left.
                Err(e) if e.errno() == libc::ECHILD => return true,
                Err(e) => {
                    warn!("error while waiting for children: {:?}", e);
                    return false;
                }
                // We reaped one child, so continue reaping.
                _ => {},
            }
        }
        // There's no timeout option for waitpid which reap_child calls internally, so our only
        // recourse is to sleep while waiting for the children to exit.
        sleep(Duration::from_millis(CHILD_WAIT_MS));
    }

    // If we've made it to this point, not all of the children have exited.
    return false;
}

fn set_argument(cfg: &mut Config, name: &str, value: Option<&str>) -> argument::Result<()> {
    match name {
        "" => {
            if cfg.plugin.is_some() {
                return Err(argument::Error::TooManyArguments("`plugin` can not be used with kernel"
                                                                 .to_owned()));
            } else if !cfg.kernel_path.as_os_str().is_empty() {
                return Err(argument::Error::TooManyArguments("expected exactly one kernel path"
                                                                 .to_owned()));
            } else {
                let kernel_path = PathBuf::from(value.unwrap());
                if !kernel_path.exists() {
                    return Err(argument::Error::InvalidValue {
                                   value: value.unwrap().to_owned(),
                                   expected: "this kernel path does not exist",
                               });
                }
                cfg.kernel_path = kernel_path;
            }
        }
        "params" => {
            cfg.params.push(value.unwrap().to_owned());
        }
        "cpus" => {
            if cfg.vcpu_count.is_some() {
                return Err(argument::Error::TooManyArguments("`cpus` already given".to_owned()));
            }
            cfg.vcpu_count =
                Some(value
                         .unwrap()
                         .parse()
                         .map_err(|_| {
                                      argument::Error::InvalidValue {
                                          value: value.unwrap().to_owned(),
                                          expected: "this value for `cpus` needs to be integer",
                                      }
                                  })?)
        }
        "mem" => {
            if cfg.memory.is_some() {
                return Err(argument::Error::TooManyArguments("`mem` already given".to_owned()));
            }
            cfg.memory =
                Some(value
                         .unwrap()
                         .parse()
                         .map_err(|_| {
                                      argument::Error::InvalidValue {
                                          value: value.unwrap().to_owned(),
                                          expected: "this value for `mem` needs to be integer",
                                      }
                                  })?)
        }
        "root" | "disk" | "rwdisk" | "qcow" | "rwqcow" => {
            let disk_path = PathBuf::from(value.unwrap());
            if !disk_path.exists() {
                return Err(argument::Error::InvalidValue {
                               value: value.unwrap().to_owned(),
                               expected: "this disk path does not exist",
                           });
            }
            if name == "root" {
                if cfg.virtio_dev_info.disks.len() >= 26 {
                    return Err(argument::Error::TooManyArguments("ran out of letters for to assign to root disk".to_owned()));
                }
                cfg.params
                    .push(format!("root=/dev/vd{} ro",
                                  char::from('a' as u8 + cfg.virtio_dev_info.disks.len() as u8)));
            }
            cfg.virtio_dev_info.disks
                .push(DiskOption {
                          path: disk_path,
                          writable: name.starts_with("rw"),
                          disk_type: if name.ends_with("qcow") {
                                  DiskType::Qcow
                              } else {
                                  DiskType::FlatFile
                              },
                      });
        }
        "host_ip" => {
            if cfg.virtio_dev_info.host_ip.is_some() {
                return Err(argument::Error::TooManyArguments("`host_ip` already given".to_owned()));
            }
            cfg.virtio_dev_info.host_ip =
                Some(value
                         .unwrap()
                         .parse()
                         .map_err(|_| {
                                      argument::Error::InvalidValue {
                                          value: value.unwrap().to_owned(),
                                          expected: "`host_ip` needs to be in the form \"x.x.x.x\"",
                                      }
                                  })?)
        }
        "netmask" => {
            if cfg.virtio_dev_info.netmask.is_some() {
                return Err(argument::Error::TooManyArguments("`netmask` already given".to_owned()));
            }
            cfg.virtio_dev_info.netmask =
                Some(value
                         .unwrap()
                         .parse()
                         .map_err(|_| {
                                      argument::Error::InvalidValue {
                                          value: value.unwrap().to_owned(),
                                          expected: "`netmask` needs to be in the form \"x.x.x.x\"",
                                      }
                                  })?)
        }
        "mac" => {
            if cfg.virtio_dev_info.mac_address.is_some() {
                return Err(argument::Error::TooManyArguments("`mac` already given".to_owned()));
            }
            cfg.virtio_dev_info.mac_address =
                Some(value
                         .unwrap()
                         .parse()
                         .map_err(|_| {
                                      argument::Error::InvalidValue {
                                          value: value.unwrap().to_owned(),
                                          expected: "`mac` needs to be in the form \"XX:XX:XX:XX:XX:XX\"",
                                      }
                                  })?)
        }
        "wayland-sock" => {
            if cfg.virtio_dev_info.wayland_socket_path.is_some() {
                return Err(argument::Error::TooManyArguments("`wayland-sock` already given"
                                                                 .to_owned()));
            }
            let wayland_socket_path = PathBuf::from(value.unwrap());
            if !wayland_socket_path.exists() {
                return Err(argument::Error::InvalidValue {
                               value: value.unwrap().to_string(),
                               expected: "Wayland socket does not exist",
                           });
            }
            cfg.virtio_dev_info.wayland_socket_path = Some(wayland_socket_path);
        }
        #[cfg(feature = "wl-dmabuf")]
        "wayland-dmabuf" => {
            cfg.virtio_dev_info.wayland_dmabuf = true
        }
        "socket" => {
            if cfg.socket_path.is_some() {
                return Err(argument::Error::TooManyArguments("`socket` already given".to_owned()));
            }
            let mut socket_path = PathBuf::from(value.unwrap());
            if socket_path.is_dir() {
                socket_path.push(format!("crosvm-{}.sock", getpid()));
            }
            if socket_path.exists() {
                return Err(argument::Error::InvalidValue {
                               value: socket_path.to_string_lossy().into_owned(),
                               expected: "this socket path already exists",
                           });
            }
            cfg.socket_path = Some(socket_path);
        }
        "multiprocess" => {
            cfg.virtio_dev_info.multiprocess = true;
        }
        "disable-sandbox" => {
            cfg.virtio_dev_info.multiprocess = false;
        }
        "cid" => {
            if cfg.virtio_dev_info.cid.is_some() {
                return Err(argument::Error::TooManyArguments("`cid` alread given".to_owned()));
            }
            cfg.virtio_dev_info.cid = Some(value.unwrap().parse().map_err(|_| {
                argument::Error::InvalidValue {
                    value: value.unwrap().to_owned(),
                    expected: "this value for `cid` must be an unsigned integer",
                }
            })?);
        }
        "seccomp-policy-dir" => {
            // `value` is Some because we are in this match so it's safe to unwrap.
            cfg.virtio_dev_info.seccomp_policy_dir = PathBuf::from(value.unwrap());
        },
        "plugin" => {
            if !cfg.kernel_path.as_os_str().is_empty() {
                return Err(argument::Error::TooManyArguments("`plugin` can not be used with kernel".to_owned()));
            } else if cfg.plugin.is_some() {
                return Err(argument::Error::TooManyArguments("`plugin` already given".to_owned()));
            }
            let plugin = PathBuf::from(value.unwrap().to_owned());
            if plugin.is_relative() {
                return Err(argument::Error::InvalidValue {
                  value: plugin.to_string_lossy().into_owned(),
                  expected: "the plugin path must be an absolute path",
                })
            }
            cfg.plugin = Some(plugin);
        },
        "plugin-root" => {
            cfg.plugin_root = Some(PathBuf::from(value.unwrap().to_owned()));
        },
        "vhost-net" => {
            cfg.virtio_dev_info.vhost_net = true
        },
        "tap-fd" => {
            if cfg.virtio_dev_info.tap_fd.is_some() {
                return Err(argument::Error::TooManyArguments("`tap-fd` alread given".to_owned()));
            }
            cfg.virtio_dev_info.tap_fd = Some(value.unwrap().parse().map_err(|_| {
                argument::Error::InvalidValue {
                    value: value.unwrap().to_owned(),
                    expected: "this value for `tap-fd` must be an unsigned integer",
                }
            })?);
        }
        "help" => return Err(argument::Error::PrintHelp),
        _ => unreachable!(),
    }
    Ok(())
}


fn run_vm(args: std::env::Args) -> std::result::Result<(), ()> {
    let arguments =
        &[Argument::positional("KERNEL", "bzImage of kernel to run"),
          Argument::short_value('p',
                                "params",
                                "PARAMS",
                                "Extra kernel or plugin command line arguments. Can be given more than once."),
          Argument::short_value('c', "cpus", "N", "Number of VCPUs. (default: 1)"),
          Argument::short_value('m',
                                "mem",
                                "N",
                                "Amount of guest memory in MiB. (default: 256)"),
          Argument::short_value('r',
                                "root",
                                "PATH",
                                "Path to a root disk image. Like `--disk` but adds appropriate kernel command line option."),
          Argument::short_value('d', "disk", "PATH", "Path to a disk image."),
          Argument::value("qcow", "PATH", "Path to a qcow2 disk image."),
          Argument::value("rwdisk", "PATH", "Path to a writable disk image."),
          Argument::value("rwqcow", "PATH", "Path to a writable qcow2 disk image."),
          Argument::value("host_ip",
                          "IP",
                          "IP address to assign to host tap interface."),
          Argument::value("netmask", "NETMASK", "Netmask for VM subnet."),
          Argument::value("mac", "MAC", "MAC address for VM."),
          Argument::value("wayland-sock", "PATH", "Path to the Wayland socket to use."),
          Argument::value("wayland-group",
                          "GROUP",
                          "Name of the group with access to the Wayland socket."),
          #[cfg(feature = "wl-dmabuf")]
          Argument::flag("wayland-dmabuf", "Enable support for DMABufs in Wayland device."),
          Argument::short_value('s',
                                "socket",
                                "PATH",
                                "Path to put the control socket. If PATH is a directory, a name will be generated."),
          Argument::short_flag('u', "multiprocess", "Run each device in a child process(default)."),
          Argument::flag("disable-sandbox", "Run all devices in one, non-sandboxed process."),
          Argument::value("cid", "CID", "Context ID for virtual sockets"),
          Argument::value("seccomp-policy-dir", "PATH", "Path to seccomp .policy files."),
          #[cfg(feature = "plugin")]
          Argument::value("plugin", "PATH", "Absolute path to plugin process to run under crosvm."),
          Argument::value("plugin-root", "PATH", "Absolute path to a directory that will become root filesystem for the plugin process."),
          Argument::flag("vhost-net", "Use vhost for networking."),
          Argument::value("tap-fd",
                          "fd",
                          "File descriptor for configured tap device.  Mutually exclusive with `host_ip`, `netmask`, and `mac`."),
          Argument::short_flag('h', "help", "Print help message.")];

    let mut cfg = Config::default();
    let match_res = set_arguments(args, &arguments[..], |name, value| set_argument(&mut cfg, name, value)).and_then(|_| {
        if cfg.kernel_path.as_os_str().is_empty() && cfg.plugin.is_none() {
            return Err(argument::Error::ExpectedArgument("`KERNEL`".to_owned()));
        }
        if cfg.virtio_dev_info.host_ip.is_some() || cfg.virtio_dev_info.netmask.is_some()
                || cfg.virtio_dev_info.mac_address.is_some() {
            if cfg.virtio_dev_info.host_ip.is_none() {
                return Err(argument::Error::ExpectedArgument("`host_ip` missing from network config".to_owned()));
            }
            if cfg.virtio_dev_info.netmask.is_none() {
                return Err(argument::Error::ExpectedArgument("`netmask` missing from network config".to_owned()));
            }
            if cfg.virtio_dev_info.mac_address.is_none() {
                return Err(argument::Error::ExpectedArgument("`mac` missing from network config".to_owned()));
            }
        }
        if cfg.plugin_root.is_some() && cfg.plugin.is_none() {
            return Err(argument::Error::ExpectedArgument("`plugin-root` requires `plugin`".to_owned()));
        }
        if cfg.virtio_dev_info.tap_fd.is_some() && (cfg.virtio_dev_info.host_ip.is_some() ||
                                                    cfg.virtio_dev_info.netmask.is_some() ||
                                                    cfg.virtio_dev_info.mac_address.is_some()) {
            return Err(argument::Error::TooManyArguments(
                "`tap_fd` and any of `host_ip`, `netmask`, or `mac` are mutually exclusive".to_owned()));
        }
        Ok(())
    });

    match match_res {
        #[cfg(feature = "plugin")]
        Ok(()) if cfg.plugin.is_some() => {
            match plugin::run_config(cfg) {
                Ok(_) => {
                    info!("crosvm and plugin have exited normally");
                    Ok(())
                }
                Err(e) => {
                    error!("{}", e);
                    Err(())
                }
            }
        }
        Ok(()) => {
            match linux::run_config(cfg) {
                Ok(_) => {
                    info!("crosvm has exited normally");
                    Ok(())
                }
                Err(e) => {
                    error!("{}", e);
                    Err(())
                }
            }
        }
        Err(argument::Error::PrintHelp) => {
            print_help("crosvm run", "KERNEL", &arguments[..]);
            Ok(())
        }
        Err(e) => {
            println!("{}", e);
            Err(())
        }
    }
}

fn stop_vms(args: std::env::Args) -> std::result::Result<(), ()> {
    let mut scm = Scm::new(1);
    if args.len() == 0 {
        print_help("crosvm stop", "VM_SOCKET...", &[]);
        println!("Stops the crosvm instance listening on each `VM_SOCKET` given.");
    }

    let mut return_result = Ok(());
    for socket_path in args {
        match UnixDatagram::unbound().and_then(|s| {
                                                   s.connect(&socket_path)?;
                                                   Ok(s)
                                               }) {
            Ok(s) => {
                if let Err(e) = VmRequest::Exit.send(&mut scm, &s) {
                    error!("failed to send stop request to socket at '{}': {:?}",
                           socket_path,
                           e);
                }
            }
            Err(e) => {
                error!("failed to connect to socket at '{}': {}", socket_path, e);
                return_result = Err(());;
            }
        }
    }

    return_result
}

fn balloon_vms(mut args: std::env::Args) -> std::result::Result<(), ()> {
    let mut scm = Scm::new(1);
    if args.len() < 2 {
        print_help("crosvm balloon", "PAGE_ADJUST VM_SOCKET...", &[]);
        println!("Adjust the ballon size of the crosvm instance by `PAGE_ADJUST` pages, `PAGE_ADJUST` can be negative to shrink the balloon.");
    }
    let num_pages: i32 = match args.nth(0).unwrap().parse::<i32>() {
        Ok(n) => n,
        Err(_) => {
            error!("Failed to parse number of pages");
            return Err(());
        },
    };

    let mut return_result = Ok(());
    for socket_path in args {
        match UnixDatagram::unbound().and_then(|s| {
                                                   s.connect(&socket_path)?;
                                                   Ok(s)
                                               }) {
            Ok(s) => {
                if let Err(e) = VmRequest::BalloonAdjust(num_pages).send(&mut scm, &s) {
                    error!("failed to send balloon request to socket at '{}': {:?}",
                           socket_path,
                           e);
                }
            }
            Err(e) => {
                error!("failed to connect to socket at '{}': {}", socket_path, e);
                return_result = Err(());
            }
        }
    }

    return_result
}

fn print_usage() {
    print_help("crosvm", "[stop|run]", &[]);
    println!("Commands:");
    println!("    stop - Stops crosvm instances via their control sockets.");
    println!("    run  - Start a new crosvm instance.");
}

fn crosvm_main() -> std::result::Result<(), ()> {
    if let Err(e) = syslog::init() {
        println!("failed to initiailize syslog: {:?}", e);
        return Err(());
    }

    let mut args = std::env::args();
    if args.next().is_none() {
        error!("expected executable name");
        return Err(());
    }

    // Past this point, usage of exit is in danger of leaking zombie processes.
    let ret = match args.next().as_ref().map(|a| a.as_ref()) {
        None => {
            print_usage();
            Ok(())
        }
        Some("stop") => {
            stop_vms(args)
        }
        Some("run") => {
            run_vm(args)
        }
        Some("balloon") => {
            balloon_vms(args)
        }
        Some(c) => {
            println!("invalid subcommand: {:?}", c);
            print_usage();
            Err(())
        }
    };

    // Reap exit status from any child device processes. At this point, all devices should have been
    // dropped in the main process and told to shutdown. Try over a period of 100ms, since it may
    // take some time for the processes to shut down.
    if !wait_all_children() {
        // We gave them a chance, and it's too late.
        warn!("not all child processes have exited; sending SIGKILL");
        if let Err(e) = kill_process_group() {
            // We're now at the mercy of the OS to clean up after us.
            warn!("unable to kill all child processes: {:?}", e);
        }
    }

    // WARNING: Any code added after this point is not guaranteed to run
    // since we may forcibly kill this process (and its children) above.
    ret
}

fn main() {
    std::process::exit(if crosvm_main().is_ok() { 0 } else { 1 });
}
