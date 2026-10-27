//! The large object functions of `be-fsstubs.c`, with the descriptors of the transaction and the reads and the writes of `inv_api.c`. The data of a large object is in the catalog of the transaction, so a rollback undoes a write.

use rupg_catalog::{Catalog, LargeObjectData, MAX_LARGE_OBJECT_SIZE};
use rupg_common::{Error, Result, SqlState};
use rupg_types::Value;

use crate::sequence::check_writable;
use crate::{Call, Kernel, Session, bad_value, not_yet};

/// `INV_WRITE`.
const INV_WRITE: i32 = 0x20000;
/// `INV_READ`.
const INV_READ: i32 = 0x40000;
/// `MaxAllocSize - VARHDRSZ`: the largest result of `lo_get`.
const MAX_READ: i64 = 0x3fff_ffff - 4;

/// An open large object, a `LargeObjectDesc`.
#[derive(Clone, Debug)]
struct Descriptor {
    oid: u32,
    offset: i64,
    /// `IFS_WRLOCK`. Each descriptor has `IFS_RDLOCK`.
    write: bool,
    /// The data at the open of a descriptor that only reads, as the snapshot of the statement of the open gives it. A descriptor that writes reads the data of the transaction now.
    snapshot: Option<LargeObjectData>,
    /// The number of the open in the transaction.
    serial: u64,
}

/// `cookies` of `be-fsstubs.c`: the descriptors of the large objects that the transaction opened. A descriptor is its index.
#[derive(Clone, Debug, Default)]
pub struct Descriptors {
    open: Vec<Option<Descriptor>>,
    next: u64,
}

impl Descriptors {
    /// The number of the next open. A savepoint keeps it, so that [`Descriptors::close_since`] closes the descriptors of the subtransaction.
    pub fn serial(&self) -> u64 {
        self.next
    }

    /// `AtEOSubXact_LargeObject` for an abort: closes the descriptors whose number of the open is `serial` or more.
    pub fn close_since(&mut self, serial: u64) {
        for slot in &mut self.open {
            if slot.as_ref().is_some_and(|d| d.serial >= serial) {
                *slot = None;
            }
        }
    }

    /// `AtEOXact_LargeObject`: closes all the descriptors.
    pub fn clear(&mut self) {
        self.open.clear();
    }

    /// `newLOfd`: the lowest descriptor that is free.
    fn insert(&mut self, mut descriptor: Descriptor) -> i32 {
        descriptor.serial = self.next;
        self.next += 1;
        let at = match self.open.iter().position(Option::is_none) {
            Some(at) => at,
            None => {
                self.open.push(None);
                self.open.len() - 1
            }
        };
        self.open[at] = Some(descriptor);
        i32::try_from(at).unwrap_or(i32::MAX)
    }

    /// The open descriptor `fd`.
    fn get(&mut self, fd: i32) -> Result<&mut Descriptor> {
        usize::try_from(fd)
            .ok()
            .and_then(|at| self.open.get_mut(at))
            .and_then(Option::as_mut)
            .ok_or_else(|| {
                Error::new(
                    SqlState::UNDEFINED_OBJECT,
                    format!("invalid large-object descriptor: {fd}"),
                )
            })
    }
}

/// The kernel of a function of this module by its `prosrc`.
pub(crate) fn by_src(src: &str) -> Option<Kernel> {
    Some(match src {
        "be_lo_open" => lo_open,
        "be_lo_close" => lo_close,
        "be_loread" => loread,
        "be_lowrite" => lowrite,
        "be_lo_lseek" => |call, args| lseek(call, args, false),
        "be_lo_lseek64" => |call, args| lseek(call, args, true),
        "be_lo_tell" => |call, args| tell(call, args, false),
        "be_lo_tell64" => |call, args| tell(call, args, true),
        "be_lo_truncate" => |call, args| truncate(call, args, "lo_truncate()"),
        "be_lo_truncate64" => |call, args| truncate(call, args, "lo_truncate64()"),
        "be_lo_creat" => lo_creat,
        "be_lo_create" => lo_create,
        "be_lo_unlink" => lo_unlink,
        "be_lo_import" | "be_lo_import_with_oid" => lo_import,
        "be_lo_export" => lo_export,
        "be_lo_get" => lo_get,
        "be_lo_get_fragment" => lo_get_fragment,
        "be_lo_from_bytea" => lo_from_bytea,
        "be_lo_put" => lo_put,
        _ => return None,
    })
}

fn int_arg(args: &[Value], at: usize) -> Result<i64> {
    args.get(at).and_then(Value::as_i64).ok_or_else(bad_value)
}

fn fd_arg(args: &[Value], at: usize) -> Result<i32> {
    i32::try_from(int_arg(args, at)?).map_err(|_| bad_value())
}

fn oid_arg(args: &[Value], at: usize) -> Result<u32> {
    args.get(at).and_then(Value::as_oid).ok_or_else(bad_value)
}

fn bytes_arg(args: &[Value], at: usize) -> Result<&[u8]> {
    match args.get(at) {
        Some(Value::Bytea(bytes)) => Ok(bytes),
        _ => Err(bad_value()),
    }
}

fn text_arg(args: &[Value], at: usize) -> Result<&str> {
    args.get(at).and_then(Value::as_str).ok_or_else(bad_value)
}

/// The descriptors of the session.
fn descriptors(session: &dyn Session) -> Result<&std::cell::RefCell<Descriptors>> {
    session
        .large_object_descriptors()
        .ok_or_else(|| not_yet("a large object descriptor in a session with no server"))
}

/// `f` of the catalog of the transaction now.
fn with_latest<T>(session: &dyn Session, f: impl FnOnce(&Catalog) -> T) -> Result<T> {
    match session.latest_catalog() {
        Some(catalog) => Ok(f(&catalog)),
        None => session
            .catalog()
            .map(f)
            .ok_or_else(|| not_yet("a large object in a session with no catalog")),
    }
}

fn does_not_exist(oid: u32) -> Error {
    Error::new(SqlState::UNDEFINED_OBJECT, format!("large object {oid} does not exist"))
}

fn invalid(message: String) -> Error {
    Error::new(SqlState::INVALID_PARAMETER_VALUE, message)
}

/// `lo_compat_privileges`, which turns off the checks of the privileges.
fn compat(session: &dyn Session) -> bool {
    session.setting("lo_compat_privileges").as_deref() == Some("on")
}

/// `inv_open`: a descriptor of the large object `oid` for the mode `flags`, after the checks of its privileges.
fn inv_open(oid: u32, flags: i32, session: &dyn Session) -> Result<Descriptor> {
    let write = flags & INV_WRITE != 0;
    if !write && flags & INV_READ == 0 {
        return Err(invalid(format!("invalid flags for opening a large object: {flags}")));
    }
    let data = with_latest(session, |c| c.large_object(oid).map(|l| l.data.clone()))?;
    let Some(data) = data else { return Err(does_not_exist(oid)) };
    if !compat(session) && !crate::acl::large_object_allowed(oid, write, session)? {
        return Err(Error::new(
            SqlState::INSUFFICIENT_PRIVILEGE,
            format!("permission denied for large object {oid}"),
        ));
    }
    Ok(Descriptor { oid, offset: 0, write, snapshot: (!write).then_some(data), serial: 0 })
}

/// The data that the descriptor reads.
fn data_of(descriptor: &Descriptor, session: &dyn Session) -> Result<LargeObjectData> {
    if let Some(data) = &descriptor.snapshot {
        return Ok(data.clone());
    }
    let data = with_latest(session, |c| c.large_object(descriptor.oid).map(|l| l.data.clone()))?;
    Ok(data.unwrap_or_default())
}

/// `inv_write`: writes `bytes` at the offset of the descriptor, and moves the offset after them.
fn inv_write(descriptor: &mut Descriptor, bytes: &[u8], session: &dyn Session) -> Result<i32> {
    let n = i32::try_from(bytes.len()).map_err(|_| bad_value())?;
    if n == 0 {
        return Ok(0);
    }
    if i64::from(n) + descriptor.offset > MAX_LARGE_OBJECT_SIZE {
        return Err(invalid(format!("invalid large object write request size: {n}")));
    }
    let (oid, offset) = (descriptor.oid, descriptor.offset);
    session.change_catalog(&mut |catalog| {
        if let Some(object) = catalog.large_object_mut(oid) {
            object.data.write(offset, bytes);
        }
        Ok(())
    })?;
    descriptor.offset += i64::from(n);
    Ok(n)
}

/// `inv_seek`.
fn inv_seek(
    descriptor: &mut Descriptor,
    offset: i64,
    whence: i64,
    session: &dyn Session,
) -> Result<i64> {
    let target = match whence {
        0 => offset,
        1 => descriptor.offset.saturating_add(offset),
        2 => data_of(descriptor, session)?.size().saturating_add(offset),
        _ => return Err(invalid(format!("invalid whence setting: {whence}"))),
    };
    if !(0..=MAX_LARGE_OBJECT_SIZE).contains(&target) {
        return Err(invalid(format!("invalid large object seek target: {target}")));
    }
    descriptor.offset = target;
    Ok(target)
}

/// `LargeObjectCreate` for `inv_create`, with the current user as the owner.
fn create(oid: u32, session: &dyn Session) -> Result<u32> {
    let owner = crate::acl::current_user(session);
    let mut created = 0;
    session.change_catalog(&mut |catalog| {
        created = catalog.create_large_object(oid, owner)?;
        Ok(())
    })?;
    Ok(created)
}

/// `be_lo_open`.
fn lo_open(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (oid, mode) = (oid_arg(args, 0)?, fd_arg(args, 1)?);
    let session = call.session;
    if mode & INV_WRITE != 0 {
        check_writable("lo_open(INV_WRITE)", session)?;
    }
    let fds = descriptors(session)?;
    let descriptor = inv_open(oid, mode, session)?;
    Ok(Value::Int4(fds.borrow_mut().insert(descriptor)))
}

/// `be_lo_close`.
fn lo_close(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let fd = fd_arg(args, 0)?;
    let mut fds = descriptors(call.session)?.borrow_mut();
    fds.get(fd)?;
    fds.open[usize::try_from(fd).map_err(|_| bad_value())?] = None;
    Ok(Value::Int4(0))
}

/// `be_loread`: a negative length reads nothing.
fn loread(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (fd, len) = (fd_arg(args, 0)?, int_arg(args, 1)?);
    let session = call.session;
    let mut fds = descriptors(session)?.borrow_mut();
    let descriptor = fds.get(fd)?;
    let bytes =
        data_of(descriptor, session)?.read(descriptor.offset, usize::try_from(len).unwrap_or(0));
    descriptor.offset += i64::try_from(bytes.len()).map_err(|_| bad_value())?;
    Ok(Value::Bytea(bytes))
}

/// The descriptor `fd` for a write.
fn for_write(fds: &mut Descriptors, fd: i32) -> Result<&mut Descriptor> {
    let descriptor = fds.get(fd)?;
    if !descriptor.write {
        return Err(Error::new(
            SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
            format!("large object descriptor {fd} was not opened for writing"),
        ));
    }
    Ok(descriptor)
}

/// `be_lowrite`.
fn lowrite(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let session = call.session;
    check_writable("lowrite()", session)?;
    let fd = fd_arg(args, 0)?;
    let mut fds = descriptors(session)?.borrow_mut();
    let descriptor = for_write(&mut fds, fd)?;
    Ok(Value::Int4(inv_write(descriptor, bytes_arg(args, 1)?, session)?))
}

/// `be_lo_lseek` and `be_lo_lseek64`.
fn lseek(call: &Call<'_>, args: &[Value], wide: bool) -> Result<Value> {
    let (fd, offset, whence) = (fd_arg(args, 0)?, int_arg(args, 1)?, int_arg(args, 2)?);
    let session = call.session;
    let mut fds = descriptors(session)?.borrow_mut();
    let at = inv_seek(fds.get(fd)?, offset, whence, session)?;
    if wide {
        return Ok(Value::Int8(at));
    }
    i32::try_from(at).map(Value::Int4).map_err(|_| {
        Error::new(
            SqlState::NUMERIC_VALUE_OUT_OF_RANGE,
            format!("lo_lseek result out of range for large-object descriptor {fd}"),
        )
    })
}

/// `be_lo_tell` and `be_lo_tell64`.
fn tell(call: &Call<'_>, args: &[Value], wide: bool) -> Result<Value> {
    let fd = fd_arg(args, 0)?;
    let at = descriptors(call.session)?.borrow_mut().get(fd)?.offset;
    if wide {
        return Ok(Value::Int8(at));
    }
    i32::try_from(at).map(Value::Int4).map_err(|_| {
        Error::new(
            SqlState::NUMERIC_VALUE_OUT_OF_RANGE,
            format!("lo_tell result out of range for large-object descriptor {fd}"),
        )
    })
}

/// `be_lo_truncate` and `be_lo_truncate64`, with `inv_truncate`. The offset of the descriptor stays.
fn truncate(call: &Call<'_>, args: &[Value], command: &str) -> Result<Value> {
    let session = call.session;
    check_writable(command, session)?;
    let (fd, len) = (fd_arg(args, 0)?, int_arg(args, 1)?);
    let mut fds = descriptors(session)?.borrow_mut();
    let oid = for_write(&mut fds, fd)?.oid;
    if !(0..=MAX_LARGE_OBJECT_SIZE).contains(&len) {
        return Err(invalid(format!("invalid large object truncation target: {len}")));
    }
    session.change_catalog(&mut |catalog| {
        if let Some(object) = catalog.large_object_mut(oid) {
            object.data.truncate(len);
        }
        Ok(())
    })?;
    Ok(Value::Int4(0))
}

/// `be_lo_creat`: the mode has no use.
fn lo_creat(call: &Call<'_>, _: &[Value]) -> Result<Value> {
    check_writable("lo_creat()", call.session)?;
    Ok(Value::Oid(create(0, call.session)?))
}

/// `be_lo_create`.
fn lo_create(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    check_writable("lo_create()", call.session)?;
    Ok(Value::Oid(create(oid_arg(args, 0)?, call.session)?))
}

/// `be_lo_unlink`: the owner drops the large object, and the descriptors of the transaction for it close.
fn lo_unlink(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let session = call.session;
    check_writable("lo_unlink()", session)?;
    let oid = oid_arg(args, 0)?;
    let owner = with_latest(session, |c| c.large_object(oid).map(|l| l.owner))?;
    let Some(owner) = owner else { return Err(does_not_exist(oid)) };
    if !compat(session) && !crate::acl::owns(owner, session) {
        return Err(Error::new(
            SqlState::INSUFFICIENT_PRIVILEGE,
            format!("must be owner of large object {oid}"),
        ));
    }
    if let Some(fds) = session.large_object_descriptors() {
        for slot in &mut fds.borrow_mut().open {
            if slot.as_ref().is_some_and(|d| d.oid == oid) {
                *slot = None;
            }
        }
    }
    session.change_catalog(&mut |catalog| {
        catalog.drop_large_object(oid);
        Ok(())
    })?;
    Ok(Value::Int4(1))
}

/// The error of a file of the server as `be-fsstubs.c` words it: the text of the operating system after the name of the file.
fn file_error(action: &str, name: &str, error: &Error) -> Error {
    let quoted = format!("\"{name}\": ");
    let text = error
        .message()
        .find(&quoted)
        .map_or(error.message(), |at| &error.message()[at + quoted.len()..]);
    Error::new(error.state(), format!("could not {action} server file \"{name}\": {text}"))
}

/// `be_lo_import` and `be_lo_import_with_oid`: a new large object with the bytes of a file of the server.
fn lo_import(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let session = call.session;
    check_writable("lo_import()", session)?;
    let name = text_arg(args, 0)?;
    let oid = if args.len() > 1 { oid_arg(args, 1)? } else { 0 };
    let bytes = session.read_server_file(name).map_err(|e| file_error("open", name, &e))?;
    let oid = create(oid, session)?;
    let mut descriptor = inv_open(oid, INV_WRITE, session)?;
    inv_write(&mut descriptor, &bytes, session)?;
    Ok(Value::Oid(oid))
}

/// `be_lo_export`: writes the large object to a file of the server.
fn lo_export(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let session = call.session;
    let (oid, name) = (oid_arg(args, 0)?, text_arg(args, 1)?);
    let descriptor = inv_open(oid, INV_READ, session)?;
    let data = data_of(&descriptor, session)?;
    let size = usize::try_from(data.size()).map_err(|_| bad_value())?;
    session
        .write_server_file(name, &data.read(0, size))
        .map_err(|e| file_error("create", name, &e))?;
    Ok(Value::Int4(1))
}

/// `lo_get_fragment_internal`: `nbytes` bytes from `offset`, or the bytes to the end for -1. A read after the end gives no bytes.
fn get_fragment(oid: u32, offset: i64, nbytes: i64, session: &dyn Session) -> Result<Value> {
    let mut descriptor = inv_open(oid, INV_READ, session)?;
    let data = data_of(&descriptor, session)?;
    let size = data.size();
    let len = if size > offset {
        if nbytes >= 0 && nbytes <= size - offset { nbytes } else { size - offset }
    } else {
        0
    };
    if len > MAX_READ {
        return Err(Error::new(
            SqlState::PROGRAM_LIMIT_EXCEEDED,
            "large object read request is too large",
        ));
    }
    if len <= 0 {
        return Ok(Value::Bytea(Vec::new()));
    }
    inv_seek(&mut descriptor, offset, 0, session)?;
    Ok(Value::Bytea(data.read(offset, usize::try_from(len).map_err(|_| bad_value())?)))
}

/// `be_lo_get`.
fn lo_get(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    get_fragment(oid_arg(args, 0)?, 0, -1, call.session)
}

/// `be_lo_get_fragment`.
fn lo_get_fragment(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let nbytes = int_arg(args, 2)?;
    if nbytes < 0 {
        return Err(invalid("requested length cannot be negative".into()));
    }
    get_fragment(oid_arg(args, 0)?, int_arg(args, 1)?, nbytes, call.session)
}

/// `be_lo_from_bytea`.
fn lo_from_bytea(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let session = call.session;
    check_writable("lo_from_bytea()", session)?;
    let oid = create(oid_arg(args, 0)?, session)?;
    let mut descriptor = inv_open(oid, INV_WRITE, session)?;
    inv_write(&mut descriptor, bytes_arg(args, 1)?, session)?;
    Ok(Value::Oid(oid))
}

/// `be_lo_put`.
fn lo_put(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let session = call.session;
    check_writable("lo_put()", session)?;
    let mut descriptor = inv_open(oid_arg(args, 0)?, INV_WRITE, session)?;
    inv_seek(&mut descriptor, int_arg(args, 1)?, 0, session)?;
    inv_write(&mut descriptor, bytes_arg(args, 2)?, session)?;
    Ok(Value::Text(String::new()))
}
