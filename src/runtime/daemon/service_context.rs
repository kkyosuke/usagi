//! Persistent macOS service lookup for daemon-owned processes.
//!
//! A detached process still inherits its caller's Mach bootstrap namespace.
//! That login namespace can disappear while the daemon and its PTYs survive.
//! Select the effective user's Background namespace before starting workers.

use std::ffi::c_void;
use std::io;

use usagi_cli::cli::DaemonCommand;

pub(super) fn prepare_with(
    command: &DaemonCommand,
    select: &mut dyn FnMut() -> io::Result<()>,
) -> io::Result<()> {
    if matches!(
        command,
        DaemonCommand::Serve { .. } | DaemonCommand::BootstrapBroker
    ) {
        select()?;
    }
    Ok(())
}

/// Each successful lookup owns one send-right reference, even when Mach
/// returns the same numeric port name as an already owned reference.
trait BootstrapContext {
    fn root(&mut self, current: u32) -> io::Result<u32>;
    fn user(&mut self, root: u32, uid: u32) -> io::Result<u32>;
    fn install(&mut self, user: u32) -> io::Result<()>;
    fn release(&mut self, port: u32);
}

fn select_user_context(
    context: &mut dyn BootstrapContext,
    current: u32,
    uid: u32,
) -> io::Result<()> {
    let root = context.root(current)?;
    let user = context.user(root, uid);
    context.release(root);
    let user = user?;
    if let Err(error) = context.install(user) {
        context.release(user);
        return Err(error);
    }
    context.release(current);
    Ok(())
}

fn kernel_result(code: i32, operation: &str) -> io::Result<()> {
    if code == 0 {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "macOS daemon service context: {operation} failed (Mach status {code})"
        )))
    }
}

fn available_symbol(pointer: *mut c_void, name: &str) -> io::Result<*mut c_void> {
    if pointer.is_null() {
        Err(io::Error::other(format!(
            "macOS daemon service context: {name} is unavailable"
        )))
    } else {
        Ok(pointer)
    }
}

#[cfg(target_os = "macos")]
pub(super) mod real_io {
    #![coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=macos_daemon_ptys_use_persistent_user_service_context,runtime::daemon::service_context::tests

    use super::{BootstrapContext, available_symbol, io, kernel_result, select_user_context};

    type GetRoot = unsafe extern "C" fn(u32, *mut u32) -> i32;
    type LookupUser = unsafe extern "C" fn(u32, *const libc::c_char, libc::uid_t, *mut u32) -> i32;

    unsafe extern "C" {
        static mut bootstrap_port: u32;
        static mach_task_self_: u32;
        fn task_set_special_port(task: u32, which: libc::c_int, port: u32) -> i32;
        fn mach_port_deallocate(task: u32, port: u32) -> i32;
    }

    struct MacBootstrap {
        task: u32,
        get_root: GetRoot,
        lookup_user: LookupUser,
    }

    pub(in super::super) fn select() -> io::Result<()> {
        // Resolve the private lookup APIs before changing any process state.
        // Kernel port APIs and libSystem's cached bootstrap port are stable;
        // the two lookup symbols are checked so missing SPI fails explicitly.
        let root = available_symbol(
            unsafe { libc::dlsym(libc::RTLD_DEFAULT, c"bootstrap_get_root".as_ptr()) },
            "bootstrap_get_root",
        )?;
        let user = available_symbol(
            unsafe { libc::dlsym(libc::RTLD_DEFAULT, c"bootstrap_look_up_per_user".as_ptr()) },
            "bootstrap_look_up_per_user",
        )?;
        // SAFETY: The symbols above have the Darwin bootstrap.h ABI, and
        // libSystem remains loaded for the entire lifetime of these pointers.
        let mut context = MacBootstrap {
            task: unsafe { mach_task_self_ },
            get_root: unsafe { std::mem::transmute::<*mut libc::c_void, GetRoot>(root) },
            lookup_user: unsafe { std::mem::transmute::<*mut libc::c_void, LookupUser>(user) },
        };
        // SAFETY: Called once at daemon/broker startup, before any worker or
        // cached directory-service lookup. geteuid only reads credentials.
        let current = unsafe { bootstrap_port };
        let uid = unsafe { libc::geteuid() };
        select_user_context(&mut context, current, uid)
    }

    impl BootstrapContext for MacBootstrap {
        fn root(&mut self, current: u32) -> io::Result<u32> {
            let mut root = 0;
            // SAFETY: root is a live output pointer for this bootstrap call.
            kernel_result(
                unsafe { (self.get_root)(current, &raw mut root) },
                "bootstrap_get_root",
            )?;
            Ok(root)
        }

        fn user(&mut self, root: u32, uid: u32) -> io::Result<u32> {
            let mut user = 0;
            // SAFETY: A null service name requests the per-user namespace,
            // rather than a service within it; user is a live output pointer.
            kernel_result(
                unsafe { (self.lookup_user)(root, std::ptr::null(), uid, &raw mut user) },
                "bootstrap_look_up_per_user",
            )?;
            Ok(user)
        }

        fn install(&mut self, user: u32) -> io::Result<()> {
            // SAFETY: 4 is TASK_BOOTSTRAP_PORT, and user is an owned send right.
            kernel_result(
                unsafe { task_set_special_port(self.task, 4, user) },
                "task_set_bootstrap_port",
            )?;
            // SAFETY: Kernel installation succeeded. Update libSystem's
            // cache before releasing the old right or starting any worker.
            unsafe { bootstrap_port = user };
            Ok(())
        }

        fn release(&mut self, port: u32) {
            // SAFETY: Each invocation releases exactly one owned send right.
            let _ = unsafe { mach_port_deallocate(self.task, port) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_long_lived_daemon_roles_select_a_service_context() {
        let selections = std::cell::Cell::new(0);
        let mut select = || {
            selections.set(selections.get() + 1);
            Ok(())
        };
        for command in [
            DaemonCommand::Serve { standby: false },
            DaemonCommand::Serve { standby: true },
            DaemonCommand::BootstrapBroker,
        ] {
            let before = selections.get();
            prepare_with(&command, &mut select).unwrap();
            assert_eq!(selections.get(), before + 1);
            assert!(prepare_with(&command, &mut || Err(io::Error::other("unavailable"))).is_err());
        }
        for command in [
            DaemonCommand::Start,
            DaemonCommand::Status,
            DaemonCommand::Stop { force: false },
        ] {
            let before = selections.get();
            prepare_with(&command, &mut select).unwrap();
            assert_eq!(
                selections.get(),
                before,
                "clients retain their launch context"
            );
        }
    }

    struct FakeBootstrap {
        fail: Option<&'static str>,
        user: u32,
        events: Vec<String>,
    }

    impl BootstrapContext for FakeBootstrap {
        fn root(&mut self, current: u32) -> io::Result<u32> {
            self.events.push(format!("root:{current}"));
            kernel_result(i32::from(self.fail == Some("root")), "root")?;
            Ok(20)
        }
        fn user(&mut self, root: u32, uid: u32) -> io::Result<u32> {
            self.events.push(format!("user:{root}:{uid}"));
            kernel_result(i32::from(self.fail == Some("user")), "user")?;
            Ok(self.user)
        }
        fn install(&mut self, user: u32) -> io::Result<()> {
            self.events.push(format!("install:{user}"));
            kernel_result(i32::from(self.fail == Some("install")), "install")
        }
        fn release(&mut self, port: u32) {
            self.events.push(format!("release:{port}"));
        }
    }

    #[test]
    fn a_context_switch_releases_only_the_rights_it_owns() {
        let cases: &[(Option<&str>, u32, bool, &[&str])] = &[
            (
                None,
                30,
                true,
                &[
                    "root:10",
                    "user:20:501",
                    "release:20",
                    "install:30",
                    "release:10",
                ],
            ),
            (
                None,
                10,
                true,
                &[
                    "root:10",
                    "user:20:501",
                    "release:20",
                    "install:10",
                    "release:10",
                ],
            ),
            (Some("root"), 30, false, &["root:10"]),
            (
                Some("user"),
                30,
                false,
                &["root:10", "user:20:501", "release:20"],
            ),
            (
                Some("install"),
                30,
                false,
                &[
                    "root:10",
                    "user:20:501",
                    "release:20",
                    "install:30",
                    "release:30",
                ],
            ),
        ];
        for &(fail, user, success, events) in cases {
            let mut context = FakeBootstrap {
                fail,
                user,
                events: Vec::new(),
            };
            assert_eq!(select_user_context(&mut context, 10, 501).is_ok(), success);
            assert_eq!(context.events, events);
        }
    }

    #[test]
    fn missing_symbols_and_mach_errors_have_explicit_diagnostics() {
        assert!(kernel_result(0, "lookup").is_ok());
        assert!(
            kernel_result(5, "lookup")
                .unwrap_err()
                .to_string()
                .contains("lookup failed (Mach status 5)")
        );
        let pointer = std::ptr::NonNull::<c_void>::dangling().as_ptr();
        assert_eq!(available_symbol(pointer, "lookup").unwrap(), pointer);
        assert!(
            available_symbol(std::ptr::null_mut(), "lookup")
                .unwrap_err()
                .to_string()
                .contains("lookup is unavailable")
        );
    }
}
