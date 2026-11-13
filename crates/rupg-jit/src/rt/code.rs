// Lifted from rudb crates/rudb-qc-rt/src/code.rs at cd9f9676 (2026-10-05).
// Changes: a module of rupg-jit under `crate::rt`; the `memfd` of the arena is named `rupg-code`, and the test of many threads allows `std::thread::scope`.

//! Executable memory, per section 8.8 of `spec/compiler/08-backends.md`: the one [`CodeArena`]
//! every native backend's code is loaded into, the loader that resolves a backend's relocations,
//! and the epochs that decide when an extent nobody can be running is given back.
//!
//! A backend never maps memory. It hands over bytes and relocations, and [`CodeArena::load`] finds
//! an extent, copies the bytes through a writable view, patches the relocations there, makes the
//! instruction cache agree with what was written and returns a [`Code`] whose address is in an
//! executable view. The code is patched before anybody can call it and never after, which is the
//! spec's rule that there is no in-place patching of published code.
//!
//! How the two views are made depends on the platform, and it is the only part that does:
//!
//! - Linux maps one `memfd` twice, once readable and writable and once readable and executable.
//!   No page is ever writable and executable at the same address and nothing needs a syscall per
//!   load, which is why it is the default. On AArch64 the data cache is cleaned and the
//!   instruction cache invalidated over what was written, and a caller issues an `isb` before it
//!   jumps (see [`Code::call`]).
//! - Linux in strict mode maps one private view and flips each extent with `mprotect`, writable
//!   while it is loaded and executable after. That costs a syscall and a TLB shootdown per extent,
//!   and it is for deployments that forbid a writable alias of executable memory.
//! - macOS maps with `MAP_JIT` and, on arm64, switches the calling thread between writing and
//!   executing with `pthread_jit_write_protect_np`, then calls `sys_icache_invalidate`.
//!
//! Any other platform has no arena, [`CodeArena::new`] says so, and `interp` is the only tier.
//!
//! # Extents and chunks
//!
//! Memory is mapped a chunk of 1 MB at a time and handed out in extents whose size is a power of
//! two number of pages, by bumping a pointer through the newest chunk. A freed extent goes on the
//! free list of its size class and the next load of that class takes it. A chunk whose last extent
//! was freed is unmapped, unless it is the one being bumped through. Code bigger than a chunk gets a
//! chunk of its own, sized to it, which goes back to the system when the code does.
//!
//! # Epochs
//!
//! Dropping a [`Code`] does not free its extent. It retires it in the current epoch and moves the
//! epoch on. A worker holds a [`Worker`] while it runs compiled code and calls [`Worker::boundary`]
//! at every morsel boundary, which is one store of the epoch it saw. An extent retired in epoch `e`
//! is freed once every worker has announced `e + 1` or later, that is once every worker has been
//! between two morsels since the code was dropped. In safe Rust a thread running a [`Code`] holds a
//! reference to it and the drop cannot happen first, so the epochs are for what the ownership rules
//! cannot see: an entry point copied into a code cache or a state block and called through a raw
//! pointer, which is what document 09 publishes.
//!
//! Only 64 bit Linux and macOS are supported, the two the release matrix builds for, and the
//! system calls are declared here rather than taken from a `libc` crate because there are seven of
//! them.

#![allow(unsafe_code)]

use std::fmt;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

/// The size of a chunk the arena maps at a time.
pub const CHUNK: usize = 1 << 20;

/// A worker that is not running compiled code. It never holds an epoch back.
const QUIET: u64 = u64::MAX;

/// How the arena makes memory writable and then executable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Two views of one `memfd`, one writable and one executable. The Linux default.
    Dual,
    /// One view flipped between writable and executable with `mprotect`, per extent. Linux only,
    /// for deployments that forbid a writable alias of executable memory.
    Strict,
    /// One `MAP_JIT` view, switched per thread. The macOS mode.
    Jit,
}

impl Mode {
    /// The mode this platform uses unless told otherwise, or `None` where there is no executable
    /// memory to be had.
    #[must_use]
    pub fn native() -> Option<Mode> {
        if cfg!(all(target_os = "linux", target_pointer_width = "64")) {
            Some(Mode::Dual)
        } else if cfg!(all(target_os = "macos", target_pointer_width = "64")) {
            Some(Mode::Jit)
        } else {
            None
        }
    }

    /// Whether this platform can map memory in this mode.
    #[must_use]
    pub fn supported(self) -> bool {
        match self {
            Mode::Dual | Mode::Strict => {
                cfg!(all(target_os = "linux", target_pointer_width = "64"))
            }
            Mode::Jit => cfg!(all(target_os = "macos", target_pointer_width = "64")),
        }
    }
}

/// How a relocation's value is written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RelocKind {
    /// The absolute address `target + addend`, as 8 bytes little endian. What a call to a function
    /// that is not in the same code gets, from both Cranelift backends when code is not position
    /// independent, and the reason a far call needs nothing more.
    Abs8,
    /// `target + addend - address of the field`, as 4 bytes little endian, which is an x86-64
    /// `call` or a RIP relative operand. The load fails if the distance does not fit.
    PcRel4,
}

/// One place in a function's bytes that holds an address the loader fills in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reloc {
    /// Where the field starts, from the first byte of the code.
    pub offset: u32,
    /// How it is written.
    pub kind: RelocKind,
    /// The address the field names. The caller has already resolved the symbol.
    pub target: usize,
    /// Added to `target`.
    pub addend: i64,
}

/// What the arena holds, for tests and for `EXPLAIN`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Chunks mapped now.
    pub chunks: usize,
    /// Bytes mapped now, counting each chunk once however many views it has.
    pub mapped: usize,
    /// Extents holding code somebody still has.
    pub live: usize,
    /// Extents whose code was dropped and which wait for the workers to pass an epoch.
    pub retired: usize,
    /// Extents on the free lists.
    pub free: usize,
}

/// The executable memory of one process. Cloning it is cheap and gives another handle to the same
/// arena.
#[derive(Clone)]
pub struct CodeArena {
    shared: Arc<Shared>,
}

impl fmt::Debug for CodeArena {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CodeArena")
            .field("mode", &self.shared.mode)
            .field("epoch", &self.shared.epoch.load(Ordering::Relaxed))
            .field("stats", &self.stats())
            .finish()
    }
}

struct Shared {
    mode: Mode,
    /// The unit extents are made of: 4 KB, or the system page if that is bigger, because in strict
    /// mode an extent is what `mprotect` is called on.
    page: usize,
    epoch: AtomicU64,
    /// One slot per live [`Worker`], holding the epoch it last announced or [`QUIET`].
    workers: Mutex<Vec<Arc<AtomicU64>>>,
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    /// Indexed by chunk number. An unmapped chunk leaves `None` so numbers stay stable.
    chunks: Vec<Option<Chunk>>,
    /// The chunk new extents are bumped out of.
    bump: Option<usize>,
    /// Per size class, the extents that are free.
    free: Vec<Vec<Extent>>,
    /// Extents dropped in an epoch and not yet freed.
    retired: Vec<(u64, Extent)>,
    live: usize,
}

/// One mapping.
struct Chunk {
    /// The writable view. The same as `rx` in strict and `MAP_JIT` modes.
    rw: usize,
    /// The executable view.
    rx: usize,
    len: usize,
    /// How far the bump pointer has got.
    top: usize,
    /// Extents handed out of this chunk and not yet freed.
    used: usize,
    /// A chunk made for one piece of code bigger than [`CHUNK`].
    single: bool,
}

/// A run of pages inside a chunk.
#[derive(Clone, Copy, Debug)]
struct Extent {
    chunk: usize,
    offset: usize,
    len: usize,
    class: usize,
}

/// A size class too big for a chunk, whose extent is a chunk of its own.
const SINGLE: usize = usize::MAX;

impl CodeArena {
    /// The arena for this platform, in its native mode.
    ///
    /// # Errors
    ///
    /// On a platform with no executable memory, which is anything but 64 bit Linux and macOS.
    pub fn new() -> io::Result<CodeArena> {
        match Mode::native() {
            Some(mode) => CodeArena::with_mode(mode),
            None => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "this platform has no executable memory for compiled code",
            )),
        }
    }

    /// An arena in `mode`.
    ///
    /// # Errors
    ///
    /// If this platform cannot map memory in `mode`.
    pub fn with_mode(mode: Mode) -> io::Result<CodeArena> {
        if !mode.supported() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("{mode:?} executable memory is not available on this platform"),
            ));
        }
        let page = sys::page_size().max(4096);
        Ok(CodeArena {
            shared: Arc::new(Shared {
                mode,
                page,
                epoch: AtomicU64::new(1),
                workers: Mutex::new(Vec::new()),
                inner: Mutex::new(Inner::default()),
            }),
        })
    }

    /// The mode the arena maps in.
    #[must_use]
    pub fn mode(&self) -> Mode {
        self.shared.mode
    }

    /// The epoch now.
    #[must_use]
    pub fn epoch(&self) -> u64 {
        self.shared.epoch.load(Ordering::Acquire)
    }

    /// Copies `bytes` into an executable extent, with `relocs` filled in, and returns the code.
    ///
    /// # Errors
    ///
    /// If the system will not map memory, if a relocation lies outside the bytes, or if a
    /// [`RelocKind::PcRel4`] target is more than 2 GB from where the code landed. Nothing is left
    /// allocated when it fails.
    pub fn load(&self, bytes: &[u8], relocs: &[Reloc]) -> io::Result<Code> {
        let shared = &self.shared;
        let mut inner = shared.lock();
        shared.reclaim(&mut inner);
        let extent = shared.allocate(&mut inner, bytes.len().max(1))?;
        let Some((rw, rx)) = inner.chunks[extent.chunk]
            .as_ref()
            .map(|c| (c.rw + extent.offset, c.rx + extent.offset))
        else {
            return Err(io::Error::other("an extent was handed out of a chunk that is not mapped"));
        };

        // Patching happens in a copy, so the extent is written once and a relocation that does
        // not fit leaves nothing half done.
        let mut code = bytes.to_vec();
        if let Err(e) = patch(&mut code, rx, relocs) {
            shared.release(&mut inner, extent);
            return Err(e);
        }
        if let Err(e) = sys::write(shared.mode, rw, rx, extent.len, &code) {
            shared.release(&mut inner, extent);
            return Err(e);
        }
        inner.live += 1;
        drop(inner);
        Ok(Code { rx, len: bytes.len(), extent, shared: Arc::clone(shared) })
    }

    /// A worker's slot in the epochs, quiet until it announces one.
    #[must_use]
    pub fn worker(&self) -> Worker {
        let slot = Arc::new(AtomicU64::new(QUIET));
        self.shared.workers.lock().unwrap_or_else(PoisonError::into_inner).push(Arc::clone(&slot));
        Worker { slot, shared: Arc::clone(&self.shared) }
    }

    /// Frees every retired extent that no worker can still be running.
    pub fn reclaim(&self) {
        let mut inner = self.shared.lock();
        self.shared.reclaim(&mut inner);
    }

    /// What the arena holds now.
    #[must_use]
    pub fn stats(&self) -> Stats {
        let inner = self.shared.lock();
        let mapped: Vec<&Chunk> = inner.chunks.iter().filter_map(Option::as_ref).collect();
        Stats {
            chunks: mapped.len(),
            mapped: mapped.iter().map(|c| c.len).sum(),
            live: inner.live,
            retired: inner.retired.len(),
            free: inner.free.iter().map(Vec::len).sum(),
        }
    }
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The oldest epoch any worker may still be in.
    fn oldest(&self) -> u64 {
        let workers = self.workers.lock().unwrap_or_else(PoisonError::into_inner);
        workers.iter().map(|w| w.load(Ordering::Acquire)).min().unwrap_or(QUIET)
    }

    fn reclaim(&self, inner: &mut Inner) {
        if inner.retired.is_empty() {
            return;
        }
        let oldest = self.oldest();
        let (gone, kept): (Vec<_>, Vec<_>) =
            std::mem::take(&mut inner.retired).into_iter().partition(|(e, _)| *e < oldest);
        inner.retired = kept;
        for (_, extent) in gone {
            self.release(inner, extent);
        }
    }

    /// An extent of at least `len` bytes, from a free list, from the bump chunk, or from a new
    /// chunk.
    fn allocate(&self, inner: &mut Inner, len: usize) -> io::Result<Extent> {
        let pages = len.div_ceil(self.page).next_power_of_two();
        let size = pages * self.page;
        if size > CHUNK {
            let chunk = self.map(inner, size, true)?;
            if let Some(c) = inner.chunks[chunk].as_mut() {
                c.top = size;
                c.used = 1;
            }
            return Ok(Extent { chunk, offset: 0, len: size, class: SINGLE });
        }
        let class = pages.trailing_zeros() as usize;
        if inner.free.len() <= class {
            inner.free.resize_with(class + 1, Vec::new);
        }
        if let Some(extent) = inner.free[class].pop() {
            if let Some(c) = inner.chunks[extent.chunk].as_mut() {
                c.used += 1;
            }
            return Ok(extent);
        }
        let fits = inner
            .bump
            .and_then(|b| inner.chunks[b].as_ref().map(|c| (b, c)))
            .filter(|(_, c)| c.top + size <= c.len)
            .map(|(b, _)| b);
        let chunk = match fits {
            Some(b) => b,
            None => {
                let b = self.map(inner, CHUNK, false)?;
                let old = inner.bump.replace(b);
                // The chunk bumped through until now is an ordinary chunk from here on, so if
                // everything in it has already gone it goes too.
                if let Some(old) = old {
                    self.unmap_if_empty(inner, old);
                }
                b
            }
        };
        let c = inner.chunks[chunk].as_mut().expect("the bump chunk is mapped");
        let offset = c.top;
        c.top += size;
        c.used += 1;
        Ok(Extent { chunk, offset, len: size, class })
    }

    fn map(&self, inner: &mut Inner, len: usize, single: bool) -> io::Result<usize> {
        let (rw, rx) = sys::map(self.mode, len)?;
        let chunk = Chunk { rw, rx, len, top: 0, used: 0, single };
        let at = match inner.chunks.iter().position(Option::is_none) {
            Some(at) => {
                inner.chunks[at] = Some(chunk);
                at
            }
            None => {
                inner.chunks.push(Some(chunk));
                inner.chunks.len() - 1
            }
        };
        Ok(at)
    }

    /// Gives an extent back, to its free list or with its chunk to the system.
    fn release(&self, inner: &mut Inner, extent: Extent) {
        let Some(c) = inner.chunks[extent.chunk].as_mut() else { return };
        c.used -= 1;
        if extent.class != SINGLE {
            if self.mode == Mode::Strict {
                // Writable again for whoever takes it next. Failing leaves it executable, and a
                // load into it would fault on the write, so the extent is dropped instead.
                if sys::protect(c.rw + extent.offset, extent.len, true).is_err() {
                    return;
                }
            }
            inner.free[extent.class].push(extent);
        }
        self.unmap_if_empty(inner, extent.chunk);
    }

    fn unmap_if_empty(&self, inner: &mut Inner, chunk: usize) {
        let empty = inner.chunks[chunk].as_ref().is_some_and(|c| c.used == 0);
        if !empty || inner.bump == Some(chunk) {
            return;
        }
        let Some(c) = inner.chunks[chunk].take() else { return };
        if !c.single {
            for list in &mut inner.free {
                list.retain(|e| e.chunk != chunk);
            }
        }
        sys::unmap(self.mode, c.rw, c.rx, c.len);
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        let inner = self.inner.get_mut().unwrap_or_else(PoisonError::into_inner);
        while let Some(slot) = inner.chunks.pop() {
            if let Some(c) = slot {
                sys::unmap(self.mode, c.rw, c.rx, c.len);
            }
        }
    }
}

/// Fills in `relocs` in `code`, which will run at `base`.
fn patch(code: &mut [u8], base: usize, relocs: &[Reloc]) -> io::Result<()> {
    for r in relocs {
        let at = r.offset as usize;
        let width = match r.kind {
            RelocKind::Abs8 => 8,
            RelocKind::PcRel4 => 4,
        };
        let Some(field) = code.get_mut(at..at + width) else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("a relocation at {at} is outside {} bytes of code", code.len()),
            ));
        };
        let value = (r.target as i64).wrapping_add(r.addend);
        match r.kind {
            RelocKind::Abs8 => field.copy_from_slice(&value.to_le_bytes()),
            RelocKind::PcRel4 => {
                let from = (base + at) as i64;
                let delta = i32::try_from(value.wrapping_sub(from)).map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("a relative relocation at {at} is more than 2 GB from its target"),
                    )
                })?;
                field.copy_from_slice(&delta.to_le_bytes());
            }
        }
    }
    Ok(())
}

/// Code in the arena. Dropping it retires its extent.
pub struct Code {
    rx: usize,
    len: usize,
    extent: Extent,
    shared: Arc<Shared>,
}

impl fmt::Debug for Code {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Code").field("addr", &self.rx).field("len", &self.len).finish()
    }
}

impl Code {
    /// The address of the first byte, in the executable view.
    #[must_use]
    pub fn addr(&self) -> usize {
        self.rx
    }

    /// How many bytes were loaded.
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether no bytes were loaded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// How many bytes of the arena the code holds, which is its extent.
    #[must_use]
    pub fn footprint(&self) -> usize {
        self.extent.len
    }

    /// The loaded bytes, with the relocations filled in, read back through the executable view.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        // SAFETY: the executable view is readable in every mode, the extent is mapped for as long
        // as the arena is, which this holds, and nothing writes to it until this is dropped.
        unsafe { std::slice::from_raw_parts(std::ptr::with_exposed_provenance(self.rx), self.len) }
    }

    /// Calls the code as a pipeline function, `fn(state, morsel) -> status`, per section 8.3 of
    /// `spec/compiler/08-backends.md`.
    ///
    /// On AArch64 this issues an `isb` first, which is what the architecture asks of a core about
    /// to run code another core wrote. The spec wants one per tier switch rather than one per
    /// call, and a morsel is long enough that the difference does not show.
    ///
    /// # Safety
    ///
    /// The bytes must be a function with that signature and the C calling convention, and
    /// `state` and `morsel` must be what that function expects to find, alive and not otherwise
    /// borrowed for the length of the call.
    pub unsafe fn call(&self, state: *mut u8, morsel: *const u8) -> u64 {
        sys::isb();
        // SAFETY: the caller's contract says the bytes at `rx` are such a function, and the extent
        // is mapped executable for as long as `self` lives.
        let f: extern "C" fn(*mut u8, *const u8) -> u64 = unsafe {
            std::mem::transmute::<*const u8, extern "C" fn(*mut u8, *const u8) -> u64>(
                std::ptr::with_exposed_provenance(self.rx),
            )
        };
        f(state, morsel)
    }
}

impl Drop for Code {
    fn drop(&mut self) {
        let shared = &self.shared;
        let mut inner = shared.lock();
        inner.live -= 1;
        // Retired in the epoch now, and the epoch moves on so the workers have one to announce.
        let epoch = shared.epoch.fetch_add(1, Ordering::AcqRel);
        inner.retired.push((epoch, self.extent));
        shared.reclaim(&mut inner);
    }
}

/// One worker's place in the epochs. Hold one while running compiled code and drop it after.
pub struct Worker {
    slot: Arc<AtomicU64>,
    shared: Arc<Shared>,
}

impl fmt::Debug for Worker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Worker").field("announced", &self.slot.load(Ordering::Relaxed)).finish()
    }
}

impl Worker {
    /// Says this worker is between morsels, so it can be running nothing retired before now.
    pub fn boundary(&self) {
        self.slot.store(self.shared.epoch.load(Ordering::Acquire), Ordering::Release);
    }

    /// Says this worker is running no compiled code at all until its next boundary.
    pub fn quiet(&self) {
        self.slot.store(QUIET, Ordering::Release);
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        let mut workers = self.shared.workers.lock().unwrap_or_else(PoisonError::into_inner);
        workers.retain(|w| !Arc::ptr_eq(w, &self.slot));
    }
}

/// The system calls, one implementation per platform.
#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
mod sys {
    use std::ffi::{c_char, c_int, c_long, c_uint, c_void};
    use std::io;

    use super::Mode;

    const PROT_READ: c_int = 1;
    const PROT_WRITE: c_int = 2;
    const PROT_EXEC: c_int = 4;
    const MAP_SHARED: c_int = 1;
    const MAP_PRIVATE: c_int = 2;
    const MAP_ANONYMOUS: c_int = 0x20;
    const MFD_CLOEXEC: c_uint = 1;
    const SC_PAGESIZE: c_int = 30;

    unsafe extern "C" {
        fn mmap(
            addr: *mut c_void,
            len: usize,
            prot: c_int,
            flags: c_int,
            fd: c_int,
            off: i64,
        ) -> *mut c_void;
        fn munmap(addr: *mut c_void, len: usize) -> c_int;
        fn mprotect(addr: *mut c_void, len: usize, prot: c_int) -> c_int;
        fn memfd_create(name: *const c_char, flags: c_uint) -> c_int;
        fn ftruncate(fd: c_int, len: i64) -> c_int;
        fn close(fd: c_int) -> c_int;
        fn sysconf(name: c_int) -> c_long;
    }

    pub(super) fn page_size() -> usize {
        // SAFETY: `sysconf` reads a constant and has no preconditions.
        let page = unsafe { sysconf(SC_PAGESIZE) };
        usize::try_from(page).unwrap_or(4096)
    }

    fn view(len: usize, prot: c_int, flags: c_int, fd: c_int) -> io::Result<usize> {
        // SAFETY: a fresh mapping at an address the kernel picks overlaps nothing Rust owns.
        let at = unsafe { mmap(std::ptr::null_mut(), len, prot, flags, fd, 0) };
        if at.addr() == usize::MAX {
            return Err(io::Error::last_os_error());
        }
        Ok(at.expose_provenance())
    }

    /// Maps `len` bytes and returns the writable and executable views.
    pub(super) fn map(mode: Mode, len: usize) -> io::Result<(usize, usize)> {
        if mode == Mode::Strict {
            let at = view(len, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1)?;
            return Ok((at, at));
        }
        // SAFETY: the name is a NUL terminated literal.
        let fd = unsafe { memfd_create(c"rupg-code".as_ptr(), MFD_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let views = (|| {
            // SAFETY: `fd` is the descriptor just made.
            if unsafe { ftruncate(fd, len as i64) } != 0 {
                return Err(io::Error::last_os_error());
            }
            let rw = view(len, PROT_READ | PROT_WRITE, MAP_SHARED, fd)?;
            match view(len, PROT_READ | PROT_EXEC, MAP_SHARED, fd) {
                Ok(rx) => Ok((rw, rx)),
                Err(e) => {
                    unmap(Mode::Strict, rw, rw, len);
                    Err(e)
                }
            }
        })();
        // SAFETY: `fd` is ours, and the mappings keep the file alive without it.
        unsafe { close(fd) };
        views
    }

    pub(super) fn unmap(mode: Mode, rw: usize, rx: usize, len: usize) {
        // SAFETY: both views were mapped with this length by `map` and nothing refers to them any
        // more: the arena unmaps a chunk only when no code in it is live or retired.
        unsafe {
            munmap(std::ptr::with_exposed_provenance_mut(rw), len);
            if mode == Mode::Dual {
                munmap(std::ptr::with_exposed_provenance_mut(rx), len);
            }
        }
    }

    pub(super) fn protect(at: usize, len: usize, writable: bool) -> io::Result<()> {
        let prot = if writable { PROT_READ | PROT_WRITE } else { PROT_READ | PROT_EXEC };
        // SAFETY: `at` and `len` are a page aligned extent of a mapping the arena owns.
        if unsafe { mprotect(std::ptr::with_exposed_provenance_mut(at), len, prot) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Writes `code` into an extent through `rw` and makes it executable at `rx`.
    pub(super) fn write(
        mode: Mode,
        rw: usize,
        rx: usize,
        extent: usize,
        code: &[u8],
    ) -> io::Result<()> {
        // SAFETY: `rw` is the writable view of an extent of at least `code.len()` bytes that the
        // arena has just handed out, so nothing else reads or runs it.
        unsafe {
            std::ptr::copy_nonoverlapping(
                code.as_ptr(),
                std::ptr::with_exposed_provenance_mut(rw),
                code.len(),
            );
        }
        if mode == Mode::Strict {
            protect(rw, extent, false)?;
        }
        sync(rw, rx, code.len());
        Ok(())
    }

    /// Makes the instruction cache see what was written. x86-64 keeps them coherent.
    #[cfg(target_arch = "x86_64")]
    fn sync(_rw: usize, _rx: usize, _len: usize) {}

    /// Cleans the data cache to the point of unification over what was written, through the
    /// writable view, and invalidates the instruction cache over where it will run, which is the
    /// sequence `__builtin___clear_cache` is.
    #[cfg(target_arch = "aarch64")]
    fn sync(rw: usize, rx: usize, len: usize) {
        use std::arch::asm;
        let ctr: u64;
        // SAFETY: CTR_EL0 is readable at EL0 on Linux, which traps and emulates it where not.
        unsafe { asm!("mrs {}, ctr_el0", out(reg) ctr, options(nomem, nostack)) };
        let dline = 4usize << ((ctr >> 16) & 0xf);
        let iline = 4usize << (ctr & 0xf);
        let mut at = rw & !(dline - 1);
        while at < rw + len {
            // SAFETY: a cache maintenance operation on an address the arena mapped.
            unsafe { asm!("dc cvau, {}", in(reg) at, options(nostack)) };
            at += dline;
        }
        // SAFETY: a barrier.
        unsafe { asm!("dsb ish", options(nostack)) };
        let mut at = rx & !(iline - 1);
        while at < rx + len {
            // SAFETY: a cache maintenance operation on an address the arena mapped.
            unsafe { asm!("ic ivau, {}", in(reg) at, options(nostack)) };
            at += iline;
        }
        // SAFETY: barriers.
        unsafe { asm!("dsb ish", "isb", options(nostack)) };
    }

    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    fn sync(_rw: usize, _rx: usize, _len: usize) {}

    /// What a core runs before code another core wrote.
    pub(super) fn isb() {
        #[cfg(target_arch = "aarch64")]
        // SAFETY: a barrier.
        unsafe {
            std::arch::asm!("isb", options(nostack));
        }
    }
}

#[cfg(all(target_os = "macos", target_pointer_width = "64"))]
mod sys {
    use std::ffi::{c_int, c_long, c_void};
    use std::io;

    use super::Mode;

    const PROT_READ: c_int = 1;
    const PROT_WRITE: c_int = 2;
    const PROT_EXEC: c_int = 4;
    const MAP_PRIVATE: c_int = 2;
    const MAP_JIT: c_int = 0x800;
    const MAP_ANON: c_int = 0x1000;
    const SC_PAGESIZE: c_int = 29;

    unsafe extern "C" {
        fn mmap(
            addr: *mut c_void,
            len: usize,
            prot: c_int,
            flags: c_int,
            fd: c_int,
            off: i64,
        ) -> *mut c_void;
        fn munmap(addr: *mut c_void, len: usize) -> c_int;
        fn sysconf(name: c_int) -> c_long;
        fn sys_icache_invalidate(start: *mut c_void, len: usize);
    }

    #[cfg(target_arch = "aarch64")]
    unsafe extern "C" {
        fn pthread_jit_write_protect_np(enabled: c_int);
    }

    pub(super) fn page_size() -> usize {
        // SAFETY: `sysconf` reads a constant and has no preconditions.
        let page = unsafe { sysconf(SC_PAGESIZE) };
        usize::try_from(page).unwrap_or(16384)
    }

    pub(super) fn map(_mode: Mode, len: usize) -> io::Result<(usize, usize)> {
        let prot = PROT_READ | PROT_WRITE | PROT_EXEC;
        // SAFETY: a fresh mapping at an address the kernel picks overlaps nothing Rust owns.
        let at = unsafe {
            mmap(std::ptr::null_mut(), len, prot, MAP_PRIVATE | MAP_ANON | MAP_JIT, -1, 0)
        };
        if at.addr() == usize::MAX {
            return Err(io::Error::last_os_error());
        }
        let at = at.expose_provenance();
        Ok((at, at))
    }

    pub(super) fn unmap(_mode: Mode, rw: usize, _rx: usize, len: usize) {
        // SAFETY: mapped with this length by `map`, and nothing refers to it any more.
        unsafe { munmap(std::ptr::with_exposed_provenance_mut(rw), len) };
    }

    pub(super) fn protect(_at: usize, _len: usize, _writable: bool) -> io::Result<()> {
        Ok(())
    }

    pub(super) fn write(
        _mode: Mode,
        rw: usize,
        _rx: usize,
        _extent: usize,
        code: &[u8],
    ) -> io::Result<()> {
        #[cfg(target_arch = "aarch64")]
        // SAFETY: switches this thread's view of every MAP_JIT region to writable, and it is
        // switched back below before anything on this thread can run code.
        unsafe {
            pthread_jit_write_protect_np(0);
        }
        // SAFETY: `rw` is an extent of at least `code.len()` bytes the arena has just handed out.
        unsafe {
            std::ptr::copy_nonoverlapping(
                code.as_ptr(),
                std::ptr::with_exposed_provenance_mut(rw),
                code.len(),
            );
        }
        #[cfg(target_arch = "aarch64")]
        // SAFETY: back to executable for this thread.
        unsafe {
            pthread_jit_write_protect_np(1);
        }
        // SAFETY: the range was just written and is mapped.
        unsafe { sys_icache_invalidate(std::ptr::with_exposed_provenance_mut(rw), code.len()) };
        Ok(())
    }

    pub(super) fn isb() {
        #[cfg(target_arch = "aarch64")]
        // SAFETY: a barrier.
        unsafe {
            std::arch::asm!("isb", options(nostack));
        }
    }
}

/// No executable memory. [`CodeArena::with_mode`] refuses before any of these can be reached.
#[cfg(not(any(
    all(target_os = "linux", target_pointer_width = "64"),
    all(target_os = "macos", target_pointer_width = "64")
)))]
mod sys {
    use std::io;

    use super::Mode;

    fn none<T>() -> io::Result<T> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "no executable memory on this platform"))
    }

    pub(super) fn page_size() -> usize {
        4096
    }

    pub(super) fn map(_mode: Mode, _len: usize) -> io::Result<(usize, usize)> {
        none()
    }

    pub(super) fn unmap(_mode: Mode, _rw: usize, _rx: usize, _len: usize) {}

    pub(super) fn protect(_at: usize, _len: usize, _writable: bool) -> io::Result<()> {
        none()
    }

    pub(super) fn write(
        _mode: Mode,
        _rw: usize,
        _rx: usize,
        _extent: usize,
        _code: &[u8],
    ) -> io::Result<()> {
        none()
    }

    pub(super) fn isb() {}
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `fn(a, b) -> a + b` for the pipeline signature, taking the two pointers as integers.
    #[cfg(target_arch = "x86_64")]
    const ADD: &[u8] = &[0x48, 0x89, 0xf8, 0x48, 0x01, 0xf0, 0xc3];
    #[cfg(target_arch = "aarch64")]
    const ADD: &[u8] = &[0x00, 0x00, 0x01, 0x8b, 0xc0, 0x03, 0x5f, 0xd6];

    /// `fn() -> K` with `K` an eight byte field at [`CONST_AT`] that a relocation fills.
    #[cfg(target_arch = "x86_64")]
    const CONST: &[u8] = &[0x48, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0xc3];
    #[cfg(target_arch = "x86_64")]
    const CONST_AT: u32 = 2;
    #[cfg(target_arch = "aarch64")]
    const CONST: &[u8] = &[0x40, 0x00, 0x00, 0x58, 0xc0, 0x03, 0x5f, 0xd6, 0, 0, 0, 0, 0, 0, 0, 0];
    #[cfg(target_arch = "aarch64")]
    const CONST_AT: u32 = 8;

    fn arena() -> Option<CodeArena> {
        CodeArena::new().ok()
    }

    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    fn run(code: &Code, a: usize, b: usize) -> u64 {
        // SAFETY: both test functions have the pipeline signature and read neither pointer.
        unsafe {
            code.call(
                std::ptr::with_exposed_provenance_mut(a),
                std::ptr::with_exposed_provenance(b),
            )
        }
    }

    #[test]
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    fn loaded_code_runs() {
        let Some(arena) = arena() else { return };
        let code = arena.load(ADD, &[]).unwrap();
        assert_eq!(run(&code, 40, 2), 42);
        assert_eq!(code.bytes(), ADD);
    }

    #[test]
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    fn an_absolute_relocation_is_filled_in_before_the_code_runs() {
        let Some(arena) = arena() else { return };
        let reloc =
            Reloc { offset: CONST_AT, kind: RelocKind::Abs8, target: 0x1234_5678_9abc, addend: 3 };
        let code = arena.load(CONST, &[reloc]).unwrap();
        assert_eq!(run(&code, 0, 0), 0x1234_5678_9abf);
    }

    #[test]
    #[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")))]
    fn strict_mode_runs_the_same_code() {
        let arena = CodeArena::with_mode(Mode::Strict).unwrap();
        let code = arena.load(ADD, &[]).unwrap();
        assert_eq!(run(&code, 7, 8), 15);
        let at = code.addr();
        drop(code);
        // Freed and flipped back to writable, so the next load of the same size lands there.
        let again = arena.load(ADD, &[]).unwrap();
        assert_eq!(again.addr(), at);
        assert_eq!(run(&again, 1, 2), 3);
    }

    #[test]
    fn a_relative_relocation_is_measured_from_the_field() {
        let Some(arena) = arena() else { return };
        let bytes = [0u8; 16];
        let probe = arena.load(&bytes, &[]).unwrap();
        let target = probe.addr() + 100;
        let reloc = Reloc { offset: 4, kind: RelocKind::PcRel4, target, addend: -4 };
        let code = arena.load(&bytes, &[reloc]).unwrap();
        let field = i32::from_le_bytes(code.bytes()[4..8].try_into().unwrap());
        assert_eq!(code.addr() as i64 + 4 + i64::from(field), target as i64 - 4);
    }

    #[test]
    fn a_relocation_past_the_end_is_refused_and_leaves_nothing_behind() {
        let Some(arena) = arena() else { return };
        let reloc = Reloc { offset: 12, kind: RelocKind::Abs8, target: 0, addend: 0 };
        assert!(arena.load(&[0; 16], &[reloc]).is_err());
        let stats = arena.stats();
        assert_eq!((stats.live, stats.free), (0, 1));
    }

    #[test]
    fn an_extent_waits_for_every_worker_to_pass_a_boundary() {
        let Some(arena) = arena() else { return };
        let worker = arena.worker();
        worker.boundary();
        let code = arena.load(&[0xc3; 100], &[]).unwrap();
        let at = code.addr();
        drop(code);
        // The worker announced the epoch the code was dropped in, so it may still be running it.
        assert_eq!(arena.stats().retired, 1);
        let other = arena.load(&[0xc3; 100], &[]).unwrap();
        assert_ne!(other.addr(), at);
        worker.boundary();
        arena.reclaim();
        assert_eq!(arena.stats().retired, 0);
        let reused = arena.load(&[0xc3; 100], &[]).unwrap();
        assert_eq!(reused.addr(), at);
        drop(other);
        drop(reused);
        // A quiet worker holds nothing back.
        worker.quiet();
        arena.reclaim();
        assert_eq!(arena.stats().retired, 0);
    }

    #[test]
    fn without_workers_a_drop_frees_at_once() {
        let Some(arena) = arena() else { return };
        let code = arena.load(&[1; 5000], &[]).unwrap();
        assert!(code.footprint() >= 8192);
        drop(code);
        let stats = arena.stats();
        assert_eq!((stats.live, stats.retired, stats.free), (0, 0, 1));
    }

    #[test]
    fn code_bigger_than_a_chunk_gets_its_own_and_gives_it_back() {
        let Some(arena) = arena() else { return };
        let small = arena.load(&[1; 10], &[]).unwrap();
        let big = arena.load(&vec![2; CHUNK + 1], &[]).unwrap();
        assert_eq!(arena.stats().chunks, 2);
        assert_eq!(big.bytes().len(), CHUNK + 1);
        assert!(big.bytes().iter().all(|&b| b == 2));
        drop(big);
        assert_eq!(arena.stats().chunks, 1);
        drop(small);
    }

    #[test]
    fn a_chunk_that_empties_after_the_bump_moved_on_is_unmapped() {
        let Some(arena) = arena() else { return };
        let page = arena.shared.page;
        // Fill the first chunk with extents of half a chunk, then one more starts a second.
        let half = CHUNK / 2;
        let a = arena.load(&vec![0; half - page + 1], &[]).unwrap();
        let b = arena.load(&vec![0; half - page + 1], &[]).unwrap();
        let c = arena.load(&vec![0; half - page + 1], &[]).unwrap();
        assert_eq!(arena.stats().chunks, 2);
        drop(a);
        drop(b);
        let stats = arena.stats();
        assert_eq!((stats.chunks, stats.free), (1, 0));
        drop(c);
        assert_eq!(arena.stats().chunks, 1);
    }

    #[test]
    // The test needs real threads that load and drop code at the same time, which the tasks of rupg-platform do not give.
    #[allow(clippy::disallowed_methods)]
    fn code_can_be_loaded_and_dropped_from_many_threads() {
        let Some(arena) = arena() else { return };
        std::thread::scope(|s| {
            for t in 0..4u8 {
                let arena = arena.clone();
                s.spawn(move || {
                    let worker = arena.worker();
                    for i in 0..50u8 {
                        worker.boundary();
                        let bytes = vec![t ^ i; 1 + usize::from(i) * 97];
                        let code = arena.load(&bytes, &[]).unwrap();
                        assert_eq!(code.bytes(), &bytes[..]);
                    }
                });
            }
        });
        arena.reclaim();
        let stats = arena.stats();
        assert_eq!((stats.live, stats.retired), (0, 0));
    }
}
