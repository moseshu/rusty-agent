//! Compiles the Linux syscall policy and transfers it to bubblewrap without a named file.

use std::{
    collections::BTreeMap,
    io::{self, Seek as _, Write as _},
    os::fd::AsRawFd as _,
};

use seccompiler::{BpfProgram, SeccompAction, SeccompFilter};

use crate::sandbox::NetworkAccess;

pub(super) fn compile(network: NetworkAccess) -> io::Result<Vec<u8>> {
    let arch = std::env::consts::ARCH
        .try_into()
        .map_err(io::Error::other)?;
    let mut rules = BTreeMap::new();
    for syscall in [
        libc::SYS_ptrace,
        libc::SYS_process_vm_readv,
        libc::SYS_process_vm_writev,
        libc::SYS_io_uring_setup,
        libc::SYS_io_uring_enter,
        libc::SYS_io_uring_register,
    ] {
        rules.insert(syscall, vec![]);
    }
    if !network.is_allowed() {
        // Deny creation as well as use: netns alone does not confine pathname Unix sockets.
        // Unlike upstream's local socketpair exception, Denied promises no sockets at all.
        //
        // **This is stricter than the reference, and it has a cost.** codex permits `AF_UNIX`
        // through an arg-0 condition on `socket`/`socketpair`, and permits `recvfrom`, because
        // local IPC over a Unix socket is how a good deal of ordinary tooling manages its own
        // subprocesses — its comment names `cargo clippy`; Python's `multiprocessing`, `syslog()`
        // and any D-Bus client are in the same family. Under this list they get `EPERM` from deep
        // inside a toolchain, which is a hard failure to attribute. The promise being kept is the
        // simpler one: `Denied` means no sockets, not "no IP sockets". A host that needs local IPC
        // under denial is the case that would reintroduce the arg-0 exception, and it should be a
        // deliberate change with a Linux machine to test it on.
        for syscall in [
            libc::SYS_socket,
            libc::SYS_socketpair,
            libc::SYS_connect,
            libc::SYS_bind,
            libc::SYS_listen,
            libc::SYS_accept,
            libc::SYS_accept4,
            libc::SYS_sendto,
            libc::SYS_sendmsg,
            libc::SYS_sendmmsg,
            libc::SYS_recvfrom,
            libc::SYS_recvmsg,
            libc::SYS_recvmmsg,
        ] {
            rules.insert(syscall, vec![]);
        }
    }
    let filter = SeccompFilter::new(
        rules,
        SeccompAction::Allow,
        SeccompAction::Errno(1), // EPERM on supported Linux architectures.
        arch,
    )
    .map_err(io::Error::other)?;
    let program: BpfProgram = filter.try_into().map_err(io::Error::other)?;
    let mut bytes = Vec::with_capacity(program.len() * 8);
    // x32 shares AUDIT_ARCH_X86_64 with the native ABI but changes syscall numbers. Reject
    // it before the generated filter so alternate syscall numbers cannot miss the denylist.
    #[cfg(target_arch = "x86_64")]
    for (code, jt, jf, k) in [
        (0x20_u16, 0, 0, 0_u32),   // Load seccomp_data.nr.
        (0x35, 0, 1, 0x4000_0000), // Skip rejection only for native syscall numbers.
        (0x06, 0, 0, 0x0005_0001), // Return SECCOMP_RET_ERRNO | EPERM.
    ] {
        bytes.extend_from_slice(&code.to_ne_bytes());
        bytes.extend([jt, jf]);
        bytes.extend_from_slice(&k.to_ne_bytes());
    }
    for instruction in program {
        bytes.extend_from_slice(&instruction.code.to_ne_bytes());
        bytes.extend([instruction.jt, instruction.jf]);
        bytes.extend_from_slice(&instruction.k.to_ne_bytes());
    }
    Ok(bytes)
}

pub(in crate::sandbox) fn attach(
    command: &mut std::process::Command,
    filter: &[u8],
) -> io::Result<()> {
    use rustix::fs::{MemfdFlags, SealFlags, fcntl_add_seals, memfd_create};
    use rustix::io::{FdFlags, fcntl_dupfd_cloexec, fcntl_setfd};

    let fd = memfd_create(
        c"ra-seccomp",
        MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING,
    )?;
    // Keep the policy off the standard descriptors. The number below is written into argv, and the
    // child's stdio is dup2'd onto 0, 1 and 2 *before* the `pre_exec` closure runs — so a policy
    // that happened to land on one of them would be replaced by a pipe, and bubblewrap would read
    // its filter from the wrong descriptor. It needs a parent with those three closed, which is
    // unusual rather than impossible, and the outcome would be a failed launch rather than an
    // unfiltered one; this removes the case instead of relying on that.
    let fd = if fd.as_raw_fd() < 3 {
        fcntl_dupfd_cloexec(&fd, 3)?
    } else {
        fd
    };
    let mut file = std::fs::File::from(fd);
    file.write_all(filter)?;
    file.rewind()?;
    fcntl_add_seals(
        &file,
        SealFlags::WRITE | SealFlags::GROW | SealFlags::SHRINK | SealFlags::SEAL,
    )?;
    command.args(["--seccomp", &file.as_raw_fd().to_string()]);

    // SAFETY: only the async-signal-safe fcntl syscall runs after fork. The closure owns the
    // descriptor until spawn finishes; CLOEXEC is cleared only in the child, so concurrent
    // launches cannot inherit it. Bubblewrap consumes and closes it before executing the payload.
    #[allow(unsafe_code)]
    unsafe {
        std::os::unix::process::CommandExt::pre_exec(command, move || {
            fcntl_setfd(&file, FdFlags::empty()).map_err(io::Error::from)
        });
    }
    Ok(())
}
