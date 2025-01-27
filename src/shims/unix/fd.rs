//! General management of file descriptors, and support for
//! standard file descriptors (stdin/stdout/stderr).

use std::any::Any;
use std::collections::BTreeMap;
use std::io::{self, ErrorKind, IsTerminal, Read, SeekFrom, Write};

use rustc_middle::ty::TyCtxt;
use rustc_target::abi::Size;

use crate::shims::unix::*;
use crate::*;

/// Represents an open file descriptor.
pub trait FileDescriptor: std::fmt::Debug + Any {
    fn name(&self) -> &'static str;

    fn read<'tcx>(
        &mut self,
        _communicate_allowed: bool,
        _bytes: &mut [u8],
        _tcx: TyCtxt<'tcx>,
    ) -> InterpResult<'tcx, io::Result<usize>> {
        throw_unsup_format!("cannot read from {}", self.name());
    }

    fn write<'tcx>(
        &mut self,
        _communicate_allowed: bool,
        _bytes: &[u8],
        _tcx: TyCtxt<'tcx>,
    ) -> InterpResult<'tcx, io::Result<usize>> {
        throw_unsup_format!("cannot write to {}", self.name());
    }

    fn seek<'tcx>(
        &mut self,
        _communicate_allowed: bool,
        _offset: SeekFrom,
    ) -> InterpResult<'tcx, io::Result<u64>> {
        throw_unsup_format!("cannot seek on {}", self.name());
    }

    fn close<'tcx>(
        self: Box<Self>,
        _communicate_allowed: bool,
    ) -> InterpResult<'tcx, io::Result<i32>> {
        throw_unsup_format!("cannot close {}", self.name());
    }

    /// Return a new file descriptor *that refers to the same underlying object*.
    fn dup(&mut self) -> io::Result<Box<dyn FileDescriptor>>;

    fn is_tty(&self, _communicate_allowed: bool) -> bool {
        // Most FDs are not tty's and the consequence of a wrong `false` are minor,
        // so we use a default impl here.
        false
    }
}

impl dyn FileDescriptor {
    #[inline(always)]
    pub fn downcast_ref<T: Any>(&self) -> Option<&T> {
        (self as &dyn Any).downcast_ref()
    }

    #[inline(always)]
    pub fn downcast_mut<T: Any>(&mut self) -> Option<&mut T> {
        (self as &mut dyn Any).downcast_mut()
    }
}

impl FileDescriptor for io::Stdin {
    fn name(&self) -> &'static str {
        "stdin"
    }

    fn read<'tcx>(
        &mut self,
        communicate_allowed: bool,
        bytes: &mut [u8],
        _tcx: TyCtxt<'tcx>,
    ) -> InterpResult<'tcx, io::Result<usize>> {
        if !communicate_allowed {
            // We want isolation mode to be deterministic, so we have to disallow all reads, even stdin.
            helpers::isolation_abort_error("`read` from stdin")?;
        }
        Ok(Read::read(self, bytes))
    }

    fn dup(&mut self) -> io::Result<Box<dyn FileDescriptor>> {
        Ok(Box::new(io::stdin()))
    }

    fn is_tty(&self, communicate_allowed: bool) -> bool {
        communicate_allowed && self.is_terminal()
    }
}

impl FileDescriptor for io::Stdout {
    fn name(&self) -> &'static str {
        "stdout"
    }

    fn write<'tcx>(
        &mut self,
        _communicate_allowed: bool,
        bytes: &[u8],
        _tcx: TyCtxt<'tcx>,
    ) -> InterpResult<'tcx, io::Result<usize>> {
        // We allow writing to stderr even with isolation enabled.
        let result = Write::write(self, bytes);
        // Stdout is buffered, flush to make sure it appears on the
        // screen.  This is the write() syscall of the interpreted
        // program, we want it to correspond to a write() syscall on
        // the host -- there is no good in adding extra buffering
        // here.
        io::stdout().flush().unwrap();

        Ok(result)
    }

    fn dup(&mut self) -> io::Result<Box<dyn FileDescriptor>> {
        Ok(Box::new(io::stdout()))
    }

    fn is_tty(&self, communicate_allowed: bool) -> bool {
        communicate_allowed && self.is_terminal()
    }
}

impl FileDescriptor for io::Stderr {
    fn name(&self) -> &'static str {
        "stderr"
    }

    fn write<'tcx>(
        &mut self,
        _communicate_allowed: bool,
        bytes: &[u8],
        _tcx: TyCtxt<'tcx>,
    ) -> InterpResult<'tcx, io::Result<usize>> {
        // We allow writing to stderr even with isolation enabled.
        // No need to flush, stderr is not buffered.
        Ok(Write::write(&mut { self }, bytes))
    }

    fn dup(&mut self) -> io::Result<Box<dyn FileDescriptor>> {
        Ok(Box::new(io::stderr()))
    }

    fn is_tty(&self, communicate_allowed: bool) -> bool {
        communicate_allowed && self.is_terminal()
    }
}

/// Like /dev/null
#[derive(Debug)]
pub struct NullOutput;

impl FileDescriptor for NullOutput {
    fn name(&self) -> &'static str {
        "stderr and stdout"
    }

    fn write<'tcx>(
        &mut self,
        _communicate_allowed: bool,
        bytes: &[u8],
        _tcx: TyCtxt<'tcx>,
    ) -> InterpResult<'tcx, io::Result<usize>> {
        // We just don't write anything, but report to the user that we did.
        Ok(Ok(bytes.len()))
    }

    fn dup(&mut self) -> io::Result<Box<dyn FileDescriptor>> {
        Ok(Box::new(NullOutput))
    }
}

/// The file descriptor table
#[derive(Debug)]
pub struct FdTable {
    pub fds: BTreeMap<i32, Box<dyn FileDescriptor>>,
}

impl VisitProvenance for FdTable {
    fn visit_provenance(&self, _visit: &mut VisitWith<'_>) {
        // All our FileDescriptor do not have any tags.
    }
}

impl FdTable {
    pub(crate) fn new(mute_stdout_stderr: bool) -> FdTable {
        let mut fds: BTreeMap<_, Box<dyn FileDescriptor>> = BTreeMap::new();
        fds.insert(0i32, Box::new(io::stdin()));
        if mute_stdout_stderr {
            fds.insert(1i32, Box::new(NullOutput));
            fds.insert(2i32, Box::new(NullOutput));
        } else {
            fds.insert(1i32, Box::new(io::stdout()));
            fds.insert(2i32, Box::new(io::stderr()));
        }
        FdTable { fds }
    }

    pub fn insert_fd(&mut self, file_handle: Box<dyn FileDescriptor>) -> i32 {
        self.insert_fd_with_min_fd(file_handle, 0)
    }

    /// Insert a new FD that is at least `min_fd`.
    pub fn insert_fd_with_min_fd(
        &mut self,
        file_handle: Box<dyn FileDescriptor>,
        min_fd: i32,
    ) -> i32 {
        // Find the lowest unused FD, starting from min_fd. If the first such unused FD is in
        // between used FDs, the find_map combinator will return it. If the first such unused FD
        // is after all other used FDs, the find_map combinator will return None, and we will use
        // the FD following the greatest FD thus far.
        let candidate_new_fd =
            self.fds.range(min_fd..).zip(min_fd..).find_map(|((fd, _fh), counter)| {
                if *fd != counter {
                    // There was a gap in the fds stored, return the first unused one
                    // (note that this relies on BTreeMap iterating in key order)
                    Some(counter)
                } else {
                    // This fd is used, keep going
                    None
                }
            });
        let new_fd = candidate_new_fd.unwrap_or_else(|| {
            // find_map ran out of BTreeMap entries before finding a free fd, use one plus the
            // maximum fd in the map
            self.fds.last_key_value().map(|(fd, _)| fd.checked_add(1).unwrap()).unwrap_or(min_fd)
        });

        self.fds.try_insert(new_fd, file_handle).unwrap();
        new_fd
    }

    pub fn get(&self, fd: i32) -> Option<&dyn FileDescriptor> {
        Some(&**self.fds.get(&fd)?)
    }

    pub fn get_mut(&mut self, fd: i32) -> Option<&mut dyn FileDescriptor> {
        Some(&mut **self.fds.get_mut(&fd)?)
    }

    pub fn remove(&mut self, fd: i32) -> Option<Box<dyn FileDescriptor>> {
        self.fds.remove(&fd)
    }

    pub fn is_fd(&self, fd: i32) -> bool {
        self.fds.contains_key(&fd)
    }
}

impl<'mir, 'tcx: 'mir> EvalContextExt<'mir, 'tcx> for crate::MiriInterpCx<'mir, 'tcx> {}
pub trait EvalContextExt<'mir, 'tcx: 'mir>: crate::MiriInterpCxExt<'mir, 'tcx> {
    fn fcntl(&mut self, args: &[OpTy<'tcx, Provenance>]) -> InterpResult<'tcx, i32> {
        let this = self.eval_context_mut();

        if args.len() < 2 {
            throw_ub_format!(
                "incorrect number of arguments for fcntl: got {}, expected at least 2",
                args.len()
            );
        }
        let fd = this.read_scalar(&args[0])?.to_i32()?;
        let cmd = this.read_scalar(&args[1])?.to_i32()?;

        // We only support getting the flags for a descriptor.
        if cmd == this.eval_libc_i32("F_GETFD") {
            // Currently this is the only flag that `F_GETFD` returns. It is OK to just return the
            // `FD_CLOEXEC` value without checking if the flag is set for the file because `std`
            // always sets this flag when opening a file. However we still need to check that the
            // file itself is open.
            if this.machine.fds.is_fd(fd) {
                Ok(this.eval_libc_i32("FD_CLOEXEC"))
            } else {
                this.fd_not_found()
            }
        } else if cmd == this.eval_libc_i32("F_DUPFD")
            || cmd == this.eval_libc_i32("F_DUPFD_CLOEXEC")
        {
            // Note that we always assume the FD_CLOEXEC flag is set for every open file, in part
            // because exec() isn't supported. The F_DUPFD and F_DUPFD_CLOEXEC commands only
            // differ in whether the FD_CLOEXEC flag is pre-set on the new file descriptor,
            // thus they can share the same implementation here.
            if args.len() < 3 {
                throw_ub_format!(
                    "incorrect number of arguments for fcntl with cmd=`F_DUPFD`/`F_DUPFD_CLOEXEC`: got {}, expected at least 3",
                    args.len()
                );
            }
            let start = this.read_scalar(&args[2])?.to_i32()?;

            match this.machine.fds.get_mut(fd) {
                Some(file_descriptor) => {
                    let dup_result = file_descriptor.dup();
                    match dup_result {
                        Ok(dup_fd) => Ok(this.machine.fds.insert_fd_with_min_fd(dup_fd, start)),
                        Err(e) => {
                            this.set_last_error_from_io_error(e.kind())?;
                            Ok(-1)
                        }
                    }
                }
                None => this.fd_not_found(),
            }
        } else if this.tcx.sess.target.os == "macos" && cmd == this.eval_libc_i32("F_FULLFSYNC") {
            // Reject if isolation is enabled.
            if let IsolatedOp::Reject(reject_with) = this.machine.isolated_op {
                this.reject_in_isolation("`fcntl`", reject_with)?;
                this.set_last_error_from_io_error(ErrorKind::PermissionDenied)?;
                return Ok(-1);
            }

            this.ffullsync_fd(fd)
        } else {
            throw_unsup_format!("the {:#x} command is not supported for `fcntl`)", cmd);
        }
    }

    fn close(&mut self, fd_op: &OpTy<'tcx, Provenance>) -> InterpResult<'tcx, Scalar<Provenance>> {
        let this = self.eval_context_mut();

        let fd = this.read_scalar(fd_op)?.to_i32()?;

        Ok(Scalar::from_i32(if let Some(file_descriptor) = this.machine.fds.remove(fd) {
            let result = file_descriptor.close(this.machine.communicate())?;
            this.try_unwrap_io_result(result)?
        } else {
            this.fd_not_found()?
        }))
    }

    /// Function used when a file descriptor does not exist. It returns `Ok(-1)`and sets
    /// the last OS error to `libc::EBADF` (invalid file descriptor). This function uses
    /// `T: From<i32>` instead of `i32` directly because some fs functions return different integer
    /// types (like `read`, that returns an `i64`).
    fn fd_not_found<T: From<i32>>(&mut self) -> InterpResult<'tcx, T> {
        let this = self.eval_context_mut();
        let ebadf = this.eval_libc("EBADF");
        this.set_last_error(ebadf)?;
        Ok((-1).into())
    }

    fn read(
        &mut self,
        fd: i32,
        buf: Pointer<Option<Provenance>>,
        count: u64,
    ) -> InterpResult<'tcx, i64> {
        let this = self.eval_context_mut();

        // Isolation check is done via `FileDescriptor` trait.

        trace!("Reading from FD {}, size {}", fd, count);

        // Check that the *entire* buffer is actually valid memory.
        this.check_ptr_access(buf, Size::from_bytes(count), CheckInAllocMsg::MemoryAccessTest)?;

        // We cap the number of read bytes to the largest value that we are able to fit in both the
        // host's and target's `isize`. This saves us from having to handle overflows later.
        let count = count
            .min(u64::try_from(this.target_isize_max()).unwrap())
            .min(u64::try_from(isize::MAX).unwrap());
        let communicate = this.machine.communicate();

        if let Some(file_descriptor) = this.machine.fds.get_mut(fd) {
            trace!("read: FD mapped to {:?}", file_descriptor);
            // We want to read at most `count` bytes. We are sure that `count` is not negative
            // because it was a target's `usize`. Also we are sure that its smaller than
            // `usize::MAX` because it is bounded by the host's `isize`.
            let mut bytes = vec![0; usize::try_from(count).unwrap()];
            // `File::read` never returns a value larger than `count`,
            // so this cannot fail.
            let result = file_descriptor
                .read(communicate, &mut bytes, *this.tcx)?
                .map(|c| i64::try_from(c).unwrap());

            match result {
                Ok(read_bytes) => {
                    // If reading to `bytes` did not fail, we write those bytes to the buffer.
                    this.write_bytes_ptr(buf, bytes)?;
                    Ok(read_bytes)
                }
                Err(e) => {
                    this.set_last_error_from_io_error(e.kind())?;
                    Ok(-1)
                }
            }
        } else {
            trace!("read: FD not found");
            this.fd_not_found()
        }
    }

    fn write(
        &mut self,
        fd: i32,
        buf: Pointer<Option<Provenance>>,
        count: u64,
    ) -> InterpResult<'tcx, i64> {
        let this = self.eval_context_mut();

        // Isolation check is done via `FileDescriptor` trait.

        // Check that the *entire* buffer is actually valid memory.
        this.check_ptr_access(buf, Size::from_bytes(count), CheckInAllocMsg::MemoryAccessTest)?;

        // We cap the number of written bytes to the largest value that we are able to fit in both the
        // host's and target's `isize`. This saves us from having to handle overflows later.
        let count = count
            .min(u64::try_from(this.target_isize_max()).unwrap())
            .min(u64::try_from(isize::MAX).unwrap());
        let communicate = this.machine.communicate();

        let bytes = this.read_bytes_ptr_strip_provenance(buf, Size::from_bytes(count))?.to_owned();
        if let Some(file_descriptor) = this.machine.fds.get_mut(fd) {
            let result = file_descriptor
                .write(communicate, &bytes, *this.tcx)?
                .map(|c| i64::try_from(c).unwrap());
            this.try_unwrap_io_result(result)
        } else {
            this.fd_not_found()
        }
    }
}
