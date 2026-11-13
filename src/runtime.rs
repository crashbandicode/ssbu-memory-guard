//! Switch half: version and site checks, one inline hook per site (chained
//! onto another plugin's hook there through `chain`), the rule from `model`
//! applied in each hook, and a writer thread that appends what the hooks found
//! to `events.log`. Hooks allocate nothing and write no file; they hand a few
//! words to the writer through a fixed ring. Only two hook paths wait for the
//! writer, both just before the game faults or aborts anyway: an `unfixable`
//! value (so the line reaches the card first) and `abort` (the refused page
//! release or page commit, or any other caller).

use crate::chain::{self, hook::Outcome, Entry};
use crate::model::{self, Action, Block, FreeFailure, Verdict, SITES};
use core::cell::UnsafeCell;
use core::fmt::Write as _;
use skyline::hooks::{getRegionAddress, InlineCtx, Region};
use skyline::nn;
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};

const DATA_DIR: &str = "sd:/ultimate/memory-guard";
const OPEN_WRITE_APPEND: i32 = 2 | 4;
const SITE_COUNT: usize = SITES.len();

/// Counts calls into the heap. Nothing in the plugin is meant to make one.
struct CountingAllocator;
static HEAP_ALLOCATIONS: AtomicU64 = AtomicU64::new(0);

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        HEAP_ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        System.alloc(layout)
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        HEAP_ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        System.alloc_zeroed(layout)
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        HEAP_ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        System.realloc(ptr, layout, size)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout)
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

fn tick() -> u64 {
    unsafe { nn::os::GetSystemTick() as u64 }
}

fn sleep_ms(ms: u64) {
    unsafe {
        nn::os::SleepThread(nn::TimeSpan {
            nanoseconds: ms * 1_000_000,
        })
    };
}

// ---------------------------------------------------------------- kernel

/// One mapping as the kernel reports it (`svcQueryMemory`).
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct MemoryInfo {
    address: u64,
    size: u64,
    kind: u32,
    attributes: u32,
    permission: u32,
    ipc_references: u32,
    device_references: u32,
    padding: u32,
}

unsafe fn query(address: u64) -> Option<MemoryInfo> {
    let mut info = MemoryInfo::default();
    let result: u64;
    // QueryMemory: x0 = the MemoryInfo, x2 = the address; w0 = result.
    core::arch::asm!(
        "svc 0x6",
        inout("x0") &mut info as *mut MemoryInfo as u64 => result,
        lateout("x1") _,
        in("x2") address,
        clobber_abi("C"),
        options(nostack),
    );
    (result as u32 == 0).then_some(info)
}

/// Whether `len` bytes at `address` are mapped readable in one block.
fn readable(address: u64, len: u64) -> bool {
    let Some(end) = address.checked_add(len) else {
        return false;
    };
    unsafe { query(address) }.is_some_and(|info| {
        info.kind != 0
            && info.permission & 1 != 0
            && info.address <= address
            && end <= info.address.saturating_add(info.size)
    })
}

/// `svcGetInfo(id, current process)`.
unsafe fn process_info(id: u64) -> Option<u64> {
    let result: u64;
    let value: u64;
    core::arch::asm!(
        "svc 0x29",
        inout("x1") id => value,
        in("x2") 0xffff_8001u64,
        in("x3") 0u64,
        lateout("x0") result,
        clobber_abi("C"),
        options(nostack),
    );
    (result as u32 == 0).then_some(value)
}

fn classify(value: u64) -> Verdict {
    model::classify(value, |address| readable(address, model::REPAIR_SPAN))
}

// ---------------------------------------------------------------- events

const EVENT_SLOTS: usize = 32;
/// `abort` called from elsewhere (`value` the return address or its offset in
/// `.text`, `to` 1 when it is an offset).
const CODE_ABORT: u64 = 8;
/// Diag-only event: a planted stray bit (`to` is the planted value).
const CODE_INJECTED: u64 = 9;

struct EventSlot {
    seq: AtomicU64,
    tick: AtomicU64,
    site: AtomicU64,
    code: AtomicU64,
    value: AtomicU64,
    to: AtomicU64,
    slot: AtomicU64,
}

static EVENTS: [EventSlot; EVENT_SLOTS] = [const {
    EventSlot {
        seq: AtomicU64::new(0),
        tick: AtomicU64::new(0),
        site: AtomicU64::new(0),
        code: AtomicU64::new(0),
        value: AtomicU64::new(0),
        to: AtomicU64::new(0),
        slot: AtomicU64::new(0),
    }
}; EVENT_SLOTS];
static EVENT_NEXT: AtomicU64 = AtomicU64::new(0);
static EVENT_WRITTEN: AtomicU64 = AtomicU64::new(0);
static EVENT_DROPPED: AtomicU64 = AtomicU64::new(0);
static WRITER_RUNNING: AtomicBool = AtomicBool::new(false);

/// Hands one event to the writer; returns its sequence number (0 when the ring
/// is full and the event is dropped). A slot is claimed only when it is free,
/// so every claimed slot gets filled and the writer never waits on a gap.
fn push_event(site: usize, code: u64, value: u64, to: u64, slot: u64) -> u64 {
    let index = loop {
        let index = EVENT_NEXT.load(Ordering::Acquire);
        if index.wrapping_sub(EVENT_WRITTEN.load(Ordering::Acquire)) >= EVENT_SLOTS as u64 {
            EVENT_DROPPED.fetch_add(1, Ordering::Relaxed);
            return 0;
        }
        if EVENT_NEXT
            .compare_exchange_weak(index, index + 1, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            break index;
        }
    };
    let e = &EVENTS[index as usize % EVENT_SLOTS];
    e.tick.store(tick(), Ordering::Relaxed);
    e.site.store(site as u64, Ordering::Relaxed);
    e.code.store(code, Ordering::Relaxed);
    e.value.store(value, Ordering::Relaxed);
    e.to.store(to, Ordering::Relaxed);
    e.slot.store(slot, Ordering::Relaxed);
    e.seq.store(index + 1, Ordering::Release);
    index + 1
}

/// Waits (at most `ms`) until the writer has the event `seq` on the card.
fn wait_written(seq: u64, ms: u64) {
    if seq == 0 || !WRITER_RUNNING.load(Ordering::Acquire) {
        return;
    }
    for _ in 0..ms / 10 {
        if EVENT_WRITTEN.load(Ordering::Acquire) >= seq {
            return;
        }
        sleep_ms(10);
    }
}

// ---------------------------------------------------------------- the refused release

struct FreeRecord(UnsafeCell<FreeFailure>);
// SAFETY: written once by the hook that wins FREE_STATE 0 -> 1, read by the
// writer only after it sees 2.
unsafe impl Sync for FreeRecord {}
static FREE: FreeRecord = FreeRecord(UnsafeCell::new(FreeFailure {
    result: 0,
    address: 0,
    size: 0,
    total: u64::MAX,
    used: 0,
    blocks: [Block {
        address: 0,
        size: 0,
        state: 0,
        attributes: 0,
        permission: 0,
        ipc_references: 0,
        device_references: 0,
    }; model::FREE_BLOCKS],
    block_count: 0,
    forced: false,
}));
/// 0 nothing, 1 being written by the hook, 2 ready for the writer, 3 on the card.
static FREE_STATE: AtomicU8 = AtomicU8::new(0);
static FREE_TICK: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "diag")]
static FORCED: AtomicBool = AtomicBool::new(false);

/// The abort after `FreeMemoryPages` failed: what the kernel says about the
/// pages and the process, handed to the writer, then (at most 1.5 s) waited for.
unsafe fn record_free_failure(result: u32, address: u64, size: u64) {
    if FREE_STATE
        .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }
    let record = &mut *FREE.0.get();
    record.result = result;
    record.address = address;
    record.size = size;
    #[cfg(feature = "diag")]
    {
        record.forced = FORCED.load(Ordering::Acquire);
    }
    if let (Some(total), Some(used)) = (process_info(6), process_info(7)) {
        record.total = total;
        record.used = used;
    }
    let end = address.saturating_add(size.max(1));
    let mut at = address;
    while record.block_count < model::FREE_BLOCKS && at < end {
        let Some(info) = query(at) else {
            break;
        };
        record.blocks[record.block_count] = Block {
            address: info.address,
            size: info.size,
            state: info.kind,
            attributes: info.attributes,
            permission: info.permission,
            ipc_references: info.ipc_references,
            device_references: info.device_references,
        };
        record.block_count += 1;
        let next = info.address.saturating_add(info.size);
        if next <= at {
            break;
        }
        at = next;
    }
    FREE_TICK.store(tick(), Ordering::Relaxed);
    FREE_STATE.store(2, Ordering::Release);
    if WRITER_RUNNING.load(Ordering::Acquire) {
        for _ in 0..150 {
            if FREE_STATE.load(Ordering::Acquire) == 3 {
                break;
            }
            sleep_ms(10);
        }
    }
}

// ---------------------------------------------------------------- the refused commit

struct CommitRecord(UnsafeCell<model::CommitFailure>);
// SAFETY: written once by the hook that wins COMMIT_STATE 0 -> 1, read by the
// writer only after it sees 2.
unsafe impl Sync for CommitRecord {}
static COMMIT: CommitRecord = CommitRecord(UnsafeCell::new(model::CommitFailure::EMPTY));
/// 0 nothing, 1 being written by the hook, 2 ready for the writer, 3 on the card.
static COMMIT_STATE: AtomicU8 = AtomicU8::new(0);
static COMMIT_TICK: AtomicU64 = AtomicU64::new(0);
/// Set at install when the commit's instructions are 13.0.5's (`ALLOC_PAGES_RELIED`).
static COMMIT_KNOWN: AtomicBool = AtomicBool::new(false);

/// The abort after `AllocateMemoryPages` failed. The game holds its page
/// provider's mutex here, so this path takes no lock and allocates nothing:
/// registers and four `svcGetInfo` calls into a static, then (at most 1.5 s)
/// a wait for the writer, which may itself be stuck behind that mutex.
unsafe fn record_commit_failure(result: u32, address: u64, size: u64) {
    if COMMIT_STATE
        .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }
    let record = &mut *COMMIT.0.get();
    record.result = result;
    record.address = address;
    record.size = size;
    if let (Some(total), Some(used)) = (process_info(6), process_info(7)) {
        record.total = total;
        record.used = used;
    }
    if let (Some(total), Some(used)) = (process_info(16), process_info(17)) {
        record.slab_total = total;
        record.slab_used = used;
    }
    COMMIT_TICK.store(tick(), Ordering::Relaxed);
    COMMIT_STATE.store(2, Ordering::Release);
    if WRITER_RUNNING.load(Ordering::Acquire) {
        for _ in 0..150 {
            if COMMIT_STATE.load(Ordering::Acquire) == 3 {
                break;
            }
            sleep_ms(10);
        }
    }
}

// ---------------------------------------------------------------- the hooks

/// Diagnostic build: hits per site, a planted stray bit, the control run.
#[cfg(feature = "diag")]
mod diag {
    use super::*;

    const fn flags() -> [AtomicBool; SITE_COUNT] {
        [const { AtomicBool::new(false) }; SITE_COUNT]
    }
    const fn counters() -> [AtomicU64; SITE_COUNT] {
        [const { AtomicU64::new(0) }; SITE_COUNT]
    }

    pub static HITS: [AtomicU64; SITE_COUNT] = counters();
    /// Per site: plant once, on an empty or a real pointer, this bit, after
    /// this many eligible calls.
    static INJECT_ON: [AtomicBool; SITE_COUNT] = flags();
    static INJECT_EMPTY: [AtomicBool; SITE_COUNT] = flags();
    static INJECT_BIT: [AtomicU64; SITE_COUNT] = counters();
    static INJECT_AFTER: [AtomicU64; SITE_COUNT] = counters();
    static ELIGIBLE: [AtomicU64; SITE_COUNT] = counters();
    static INJECTED: [AtomicBool; SITE_COUNT] = flags();
    pub static REPAIR_OFF: AtomicBool = AtomicBool::new(false);
    pub static FORCE_AT: AtomicU64 = AtomicU64::new(u64::MAX);
    pub static RELEASES: AtomicU64 = AtomicU64::new(0);

    const OPEN_READ: i32 = 1;

    pub fn load_settings() -> model::DiagSettings {
        let mut file = path(format_args!("{DATA_DIR}/diag.txt"));
        let mut buf = [0u8; 256];
        let mut read = 0u64;
        let Some(file) = file.with_nul() else {
            return model::DiagSettings::parse(b"");
        };
        unsafe {
            let mut handle = nn::fs::FileHandle { handle: 0 };
            if nn::fs::OpenFile(&mut handle, file.as_ptr(), OPEN_READ) != 0 {
                return model::DiagSettings::parse(b"");
            }
            let ok =
                nn::fs::ReadFile1(&mut read, handle, 0, buf.as_mut_ptr(), buf.len() as u64) == 0;
            nn::fs::CloseFile(handle);
            if !ok {
                read = 0;
            }
        }
        let settings = model::DiagSettings::parse(&buf[..(read as usize).min(buf.len())]);
        for (site, injection) in settings.inject.iter().enumerate() {
            if let Some(injection) = injection {
                INJECT_EMPTY[site].store(injection.empty, Ordering::Release);
                INJECT_BIT[site].store(u64::from(injection.bit), Ordering::Release);
                INJECT_AFTER[site].store(injection.after, Ordering::Release);
                INJECT_ON[site].store(true, Ordering::Release);
            }
        }
        REPAIR_OFF.store(settings.repair_off, Ordering::Release);
        if let Some(at) = settings.force_free_failure {
            FORCE_AT.store(at, Ordering::Release);
        }
        settings
    }

    /// Whether this call should plant the stray bit (once per boot per site).
    pub fn should_inject(site: usize) -> bool {
        INJECT_ON[site].load(Ordering::Acquire) && !INJECTED[site].load(Ordering::Acquire)
    }

    /// Plants it on `value` when the mode fits; `None` leaves the call alone.
    pub fn planted(site: usize, value: u64) -> Option<u64> {
        let bit = 1u64 << INJECT_BIT[site].load(Ordering::Acquire);
        let fits = if INJECT_EMPTY[site].load(Ordering::Acquire) {
            value == 0
        } else {
            value != 0 && !model::suspicious(value)
        };
        if !fits
            || ELIGIBLE[site].fetch_add(1, Ordering::AcqRel)
                < INJECT_AFTER[site].load(Ordering::Acquire)
            || INJECTED[site].swap(true, Ordering::AcqRel)
        {
            return None;
        }
        push_event(site, CODE_INJECTED, value, value | bit, 0);
        Some(value | bit)
    }

    /// The forced refusal, just before the release call (`x0` already the
    /// address): the release that makes `FORCE_AT` asks to free address 0,
    /// which the kernel refuses, so the game takes its own abort path.
    pub unsafe fn force_release(ctx: &mut InlineCtx) {
        let at = FORCE_AT.load(Ordering::Acquire);
        if at != u64::MAX && RELEASES.fetch_add(1, Ordering::AcqRel) + 1 == at {
            FORCED.store(true, Ordering::Release);
            ctx.registers[0].set_x(0);
        }
    }
}

#[cfg(feature = "diag")]
fn repair_off() -> bool {
    diag::REPAIR_OFF.load(Ordering::Acquire)
}
#[cfg(not(feature = "diag"))]
fn repair_off() -> bool {
    false
}

/// Applies the rule to `value` seen at `site` (stored at `slot`, 0 when not in
/// memory any more): records what it found and returns the replacement.
fn judge(site: usize, value: u64, slot: u64, can_empty: bool) -> Option<u64> {
    let mut verdict = classify(value);
    if verdict == Verdict::Empty && !can_empty {
        verdict = Verdict::Unfixable;
    }
    if verdict == Verdict::Keep {
        return None;
    }
    let applied = !repair_off() && verdict.replacement().is_some();
    let (code, to) = model::verdict_code(verdict);
    let seq = push_event(site, code | if applied { 0 } else { 0x10 }, value, to, slot);
    if verdict == Verdict::Unfixable {
        // The game faults on this value next: get the line onto the card first.
        wait_written(seq, 300);
    }
    if applied {
        verdict.replacement()
    } else {
        None
    }
}

/// Swaps `old` for `new` at `slot` when it still holds `old`.
unsafe fn swap_slot(slot: u64, old: u64, new: u64) {
    if slot != 0 && slot.is_multiple_of(8) {
        let _ = (*(slot as *const AtomicU64)).compare_exchange(
            old,
            new,
            Ordering::AcqRel,
            Ordering::Relaxed,
        );
    }
}

/// A memory slot `base + offset`, when `base` looks like a pointer.
fn slot_of(base: u64, offset: u64) -> u64 {
    if base == 0 || model::suspicious(base) {
        0
    } else {
        base.wrapping_add(offset)
    }
}

unsafe fn pointer_list(site: usize, ctx: &mut InlineCtx, begin: u8, end: u8) {
    let begin = ctx.registers[begin as usize].x();
    let end = ctx.registers[end as usize].x();
    if end < begin
        || !(end - begin).is_multiple_of(8)
        || (end - begin) / 8 > 8192
        || slot_of(begin, 0) == 0
    {
        return;
    }
    let mut at = begin;
    while at < end {
        let entry = &*(at as *const AtomicU64);
        #[cfg(feature = "diag")]
        if diag::should_inject(site) {
            let value = entry.load(Ordering::Acquire);
            if let Some(planted) = diag::planted(site, value) {
                entry.store(planted, Ordering::Release);
            }
        }
        let value = entry.load(Ordering::Acquire);
        if model::suspicious(value) {
            if let Some(to) = judge(site, value, at, false) {
                swap_slot(at, value, to);
            }
        }
        at += 8;
    }
}

unsafe fn load_slot(site: usize, ctx: &mut InlineCtx, base: u8, index: Option<u8>, offset: u16) {
    let index = index.map_or(0, |index| ctx.registers[index as usize].x());
    let slot = slot_of(
        ctx.registers[base as usize].x(),
        index.wrapping_add(u64::from(offset)),
    );
    if slot == 0 || !slot.is_multiple_of(8) {
        return;
    }
    let cell = &*(slot as *const AtomicU64);
    #[cfg(feature = "diag")]
    if diag::should_inject(site) {
        if let Some(planted) = diag::planted(site, cell.load(Ordering::Acquire)) {
            cell.store(planted, Ordering::Release);
        }
    }
    let value = cell.load(Ordering::Acquire);
    if model::suspicious(value) {
        if let Some(to) = judge(site, value, slot, true) {
            swap_slot(slot, value, to);
        }
    }
}

/// The game's `.text` (set in `main`), to read return addresses as offsets.
static TEXT_BASE: AtomicU64 = AtomicU64::new(0);
static TEXT_END: AtomicU64 = AtomicU64::new(0);

/// `abort` was called: from the refused release, the record; from anywhere
/// else, where from. Either way the abort goes on after the writer has it.
unsafe fn abort_stub(site: usize, ctx: &mut InlineCtx) {
    let caller = ctx.registers[30].x();
    let base = TEXT_BASE.load(Ordering::Acquire);
    let in_main = (base..TEXT_END.load(Ordering::Acquire)).contains(&caller);
    if in_main && caller - base == model::ABORT_AFTER_RELEASE as u64 {
        record_free_failure(
            ctx.registers[0].w(),
            ctx.registers[20].x(),
            ctx.registers[21].x(),
        );
    } else if in_main
        && caller - base == model::ABORT_AFTER_COMMIT as u64
        && COMMIT_KNOWN.load(Ordering::Acquire)
    {
        record_commit_failure(
            ctx.registers[0].w(),
            ctx.registers[21].x(),
            ctx.registers[19].x(),
        );
    } else {
        let at = if in_main { caller - base } else { caller };
        let seq = push_event(site, CODE_ABORT, at, u64::from(in_main), 0);
        wait_written(seq, 300);
    }
}

unsafe fn on_site(site: usize, ctx: &mut InlineCtx) {
    #[cfg(feature = "diag")]
    diag::HITS[site].fetch_add(1, Ordering::Relaxed);
    match SITES[site].action {
        Action::PointerList { begin, end } => pointer_list(site, ctx, begin, end),
        Action::LoadSlot {
            base,
            index,
            offset,
        } => load_slot(site, ctx, base, index, offset),
        Action::AbortStub => abort_stub(site, ctx),
        Action::ForceRelease => {
            #[cfg(feature = "diag")]
            diag::force_release(ctx);
        }
    }
}

macro_rules! callbacks {
    ($($index:literal => $name:ident),* $(,)?) => {
        $(unsafe extern "C" fn $name(ctx: &mut InlineCtx) {
            on_site($index, ctx);
        })*
        const CALLBACKS: [unsafe extern "C" fn(&mut InlineCtx); SITE_COUNT] = [$($name),*];
    };
}

callbacks!(
    0 => site_0,
    1 => site_1,
    2 => site_2,
    3 => site_3,
    4 => site_4,
    5 => site_5,
    6 => site_6,
    7 => site_7,
    8 => site_8,
);

// ---------------------------------------------------------------- the log

/// A text line built in a fixed buffer; anything past `N` bytes is dropped.
struct Line<const N: usize> {
    buf: [u8; N],
    len: usize,
}

impl<const N: usize> Line<N> {
    const fn new() -> Self {
        Self {
            buf: [0; N],
            len: 0,
        }
    }

    fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }

    /// The text plus a NUL, for `nn::fs` paths; `None` when there is no room.
    fn with_nul(&mut self) -> Option<&[u8]> {
        if self.len >= N {
            return None;
        }
        self.buf[self.len] = 0;
        Some(&self.buf[..=self.len])
    }
}

impl<const N: usize> core::fmt::Write for Line<N> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let take = s.len().min(N - self.len);
        self.buf[self.len..self.len + take].copy_from_slice(&s.as_bytes()[..take]);
        self.len += take;
        Ok(())
    }
}

fn path(args: core::fmt::Arguments) -> Line<96> {
    let mut path = Line::new();
    let _ = path.write_fmt(args);
    path
}

/// Appends `bytes` to `events.log`; the file is closed after each write, so
/// it is on the card (and on the Eden host) at once.
fn append_log(bytes: &[u8]) {
    let mut file = path(format_args!("{DATA_DIR}/events.log"));
    let Some(file) = file.with_nul() else {
        return;
    };
    unsafe {
        // Fails harmlessly when the file exists.
        nn::fs::CreateFile(file.as_ptr(), 0);
        let mut handle = nn::fs::FileHandle { handle: 0 };
        if nn::fs::OpenFile(&mut handle, file.as_ptr(), OPEN_WRITE_APPEND) != 0 {
            return;
        }
        let mut offset = 0;
        if nn::fs::GetFileSize(&mut offset, handle) == 0 {
            let option = nn::fs::WriteOption { flags: 0 };
            nn::fs::WriteFile(handle, offset, bytes.as_ptr(), bytes.len() as u64, &option);
            nn::fs::FlushFile(handle);
        }
        nn::fs::CloseFile(handle);
    }
}

// ---------------------------------------------------------------- the writer thread

const WRITER_STACK: usize = 16 * 1024;

#[repr(C, align(4096))]
struct ThreadStack(UnsafeCell<[u8; WRITER_STACK]>);
#[repr(C, align(16))]
struct ThreadObject(UnsafeCell<[u8; 0x1c0]>);
// SAFETY: handed to nn::os once, at startup, and never touched again.
unsafe impl Sync for ThreadStack {}
unsafe impl Sync for ThreadObject {}
static WRITER_STACK_MEMORY: ThreadStack = ThreadStack(UnsafeCell::new([0; WRITER_STACK]));
static WRITER_THREAD: ThreadObject = ThreadObject(UnsafeCell::new([0; 0x1c0]));

/// Writes every event the hooks handed over, in order.
fn drain_events() {
    loop {
        let index = EVENT_WRITTEN.load(Ordering::Acquire);
        if index >= EVENT_NEXT.load(Ordering::Acquire) {
            return;
        }
        let e = &EVENTS[index as usize % EVENT_SLOTS];
        if e.seq.load(Ordering::Acquire) != index + 1 {
            return; // still being written by its hook
        }
        let code = e.code.load(Ordering::Relaxed);
        let (site, value, to, slot) = (
            e.site.load(Ordering::Relaxed) as usize,
            e.value.load(Ordering::Relaxed),
            e.to.load(Ordering::Relaxed),
            e.slot.load(Ordering::Relaxed),
        );
        let mut line = Line::<256>::new();
        if code == CODE_ABORT {
            let _ = model::abort_line(&mut line, e.tick.load(Ordering::Relaxed), value, to == 1);
        } else if code == CODE_INJECTED {
            let name = SITES.get(site).map_or("?", |s| s.name);
            let _ = writeln!(
                line,
                "{} diag inject site={name} value={value:#x} planted={to:#x}",
                e.tick.load(Ordering::Relaxed)
            );
        } else {
            let _ = model::event_line(
                &mut line,
                e.tick.load(Ordering::Relaxed),
                site,
                model::verdict_from_code(code & 0xf, to),
                value,
                slot,
                code & 0x10 == 0,
            );
        }
        append_log(line.as_bytes());
        EVENT_WRITTEN.store(index + 1, Ordering::Release);
    }
}

fn write_free_failure() {
    if FREE_STATE.load(Ordering::Acquire) != 2 {
        return;
    }
    let record = unsafe { &*FREE.0.get() };
    let mut text = Line::<1536>::new();
    let _ = model::free_lines(&mut text, FREE_TICK.load(Ordering::Relaxed), record);
    append_log(text.as_bytes());
    FREE_STATE.store(3, Ordering::Release);
}

fn write_commit_failure() {
    if COMMIT_STATE.load(Ordering::Acquire) != 2 {
        return;
    }
    let record = unsafe { &*COMMIT.0.get() };
    let mut line = Line::<384>::new();
    let _ = model::commit_line(&mut line, COMMIT_TICK.load(Ordering::Relaxed), record);
    append_log(line.as_bytes());
    COMMIT_STATE.store(3, Ordering::Release);
}

extern "C" fn writer_main(_: *mut skyline::libc::c_void) {
    let mut dropped = 0;
    #[cfg(feature = "diag")]
    let mut hits_logged = [0u64; SITE_COUNT];
    #[cfg(feature = "diag")]
    let mut last_hits = tick();
    loop {
        drain_events();
        write_free_failure();
        write_commit_failure();
        let now_dropped = EVENT_DROPPED.load(Ordering::Relaxed);
        if now_dropped != dropped {
            dropped = now_dropped;
            let mut line = Line::<96>::new();
            let _ = writeln!(line, "{} events_dropped total={dropped}", tick());
            append_log(line.as_bytes());
        }
        #[cfg(feature = "diag")]
        if tick().wrapping_sub(last_hits) >= 5 * 19_200_000 {
            last_hits = tick();
            let hits = diag::HITS.each_ref().map(|h| h.load(Ordering::Relaxed));
            if hits != hits_logged {
                hits_logged = hits;
                let mut line = Line::<512>::new();
                let _ = write!(line, "{} diag hits", last_hits);
                for (site, count) in SITES.iter().zip(hits) {
                    let _ = write!(line, " {}={count}", site.name);
                }
                let _ = writeln!(line);
                append_log(line.as_bytes());
            }
        }
        sleep_ms(100);
    }
}

fn start_writer() -> bool {
    unsafe {
        let thread = WRITER_THREAD.0.get().cast::<nn::os::ThreadType>();
        let priority = nn::os::GetThreadPriority(nn::os::GetCurrentThread());
        if nn::os::CreateThread(
            thread,
            writer_main,
            core::ptr::null_mut(),
            WRITER_STACK_MEMORY.0.get().cast::<u8>(),
            WRITER_STACK,
            priority,
        ) != 0
        {
            return false;
        }
        nn::os::SetThreadNamePointer(thread, c"memory-guard".as_ptr().cast());
        nn::os::StartThread(thread);
        WRITER_RUNNING.store(true, Ordering::Release);
        true
    }
}

// ---------------------------------------------------------------- install

/// `offset`'s address, when its 64 bytes are in `.text`.
unsafe fn text_address(offset: usize) -> Option<u64> {
    let base = getRegionAddress(Region::Text) as u64;
    let end = getRegionAddress(Region::Rodata) as u64;
    let address = base + offset as u64;
    (address.is_multiple_of(4) && address + 64 <= end).then_some(address)
}

/// Why a site is not hooked, when it is not.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Check {
    Usable,
    /// The site itself is neither 13.0.5 nor another plugin's hook.
    Site,
    /// An instruction the hook relies on changed.
    Relied,
    /// Not wanted in this build or with these settings.
    Off,
}

/// Reads a site and its relied-on windows, before anything is written.
unsafe fn check(site: &model::Site) -> Check {
    let Some(address) = text_address(site.offset) else {
        return Check::Site;
    };
    let words = chain::hook::words_at::<{ chain::WORDS }>(address);
    if chain::entry(&site.stock(), &words, address) == Entry::Foreign {
        return Check::Site;
    }
    for relied in site.relied {
        let Some(at) = text_address(relied.offset) else {
            return Check::Relied;
        };
        let relied_words = chain::hook::words_at::<{ chain::WORDS }>(at);
        if !chain::usable(&relied.stock(), &relied_words, at, relied.kept) {
            return Check::Relied;
        }
    }
    Check::Usable
}

fn version_is_supported() -> bool {
    let mut version = nn::oe::DisplayVersion { name: [0; 16] };
    unsafe { nn::oe::GetDisplayVersion(&mut version) };
    let end = version.name.iter().position(|v| *v == 0).unwrap_or(16);
    version.name[..end] == *model::SUPPORTED_VERSION.as_bytes()
}

/// Appends the install line to `events.log` and the Skyline log.
fn report_install(report: &mut Line<1024>) {
    let _ = write!(
        report,
        " heap_allocations={}",
        HEAP_ALLOCATIONS.load(Ordering::Relaxed)
    );
    skyline::logging::log(core::str::from_utf8(report.as_bytes()).unwrap_or(""));
    let _ = report.write_char('\n');
    for directory in ["sd:/ultimate", DATA_DIR] {
        if let Some(directory) = path(format_args!("{directory}")).with_nul() {
            unsafe { nn::fs::CreateDirectory(directory.as_ptr()) };
        }
    }
    append_log(report.as_bytes());
}

#[skyline::main(name = "memory_guard")]
pub fn main() {
    let mut report = Line::<1024>::new();
    let _ = write!(
        report,
        "{} plugin=memory-guard/{}",
        tick(),
        env!("CARGO_PKG_VERSION")
    );
    if !version_is_supported() {
        let _ = write!(report, " result=unsupported_version");
        report_install(&mut report);
        return;
    }
    unsafe {
        TEXT_BASE.store(getRegionAddress(Region::Text) as u64, Ordering::Release);
        TEXT_END.store(getRegionAddress(Region::Rodata) as u64, Ordering::Release);
    }
    #[cfg(feature = "diag")]
    let settings = {
        let settings = diag::load_settings();
        let _ = write!(report, " diag=1");
        for (site, injection) in settings.inject.iter().enumerate() {
            if let Some(injection) = injection {
                let _ = write!(
                    report,
                    " inject={}/{}/bit{}/after{}",
                    SITES[site].name,
                    if injection.empty { "empty" } else { "pointer" },
                    injection.bit,
                    injection.after
                );
            }
        }
        if settings.repair_off {
            let _ = write!(report, " repair=off");
        }
        if let Some(at) = settings.force_free_failure {
            let _ = write!(report, " force_free_failure={at}");
        }
        settings
    };
    // Every site and window is read before the first hook is written: some
    // windows hold another site of this plugin.
    let checks: [Check; SITE_COUNT] = core::array::from_fn(|i| {
        let site = &SITES[i];
        #[cfg(feature = "diag")]
        let wanted = !site.diag_only || settings.force_free_failure.is_some();
        #[cfg(not(feature = "diag"))]
        let wanted = !site.diag_only;
        if wanted {
            unsafe { check(site) }
        } else {
            Check::Off
        }
    });
    // The commit record needs the commit's own instructions, also read before
    // any hook is written; when they differ, that abort is logged by caller only.
    let commit_known = unsafe {
        text_address(model::ALLOC_PAGES_RELIED.offset).is_some_and(|at| {
            let words = chain::hook::words_at::<{ chain::WORDS }>(at);
            chain::usable(
                &model::ALLOC_PAGES_RELIED.stock(),
                &words,
                at,
                model::ALLOC_PAGES_RELIED.kept,
            )
        })
    };
    COMMIT_KNOWN.store(commit_known, Ordering::Release);
    let mut wanted = 0;
    let mut hooked = 0;
    for (i, site) in SITES.iter().enumerate() {
        match checks[i] {
            Check::Off => continue,
            Check::Site => {
                wanted += 1;
                let _ = write!(report, " {}=foreign/skipped", site.name);
            }
            Check::Relied => {
                wanted += 1;
                let _ = write!(report, " {}=relied/skipped", site.name);
            }
            Check::Usable => {
                wanted += 1;
                // One-word hooks only: a far jump would carry the
                // PC-relative instructions after the site into a trampoline.
                let (entry, outcome) = match unsafe { text_address(site.offset) } {
                    Some(address) => unsafe {
                        chain::hook::inline(address, &site.stock(), CALLBACKS[i], i, true)
                    },
                    None => (Entry::Foreign, Outcome::Skipped),
                };
                if outcome.hooked() {
                    hooked += 1;
                }
                let _ = write!(report, " {}={}/{}", site.name, entry.name(), outcome.name());
            }
        }
    }
    let result = if hooked == wanted {
        "installed"
    } else if hooked > 0 {
        "partial"
    } else {
        "not_installed"
    };
    let _ = write!(
        report,
        " alloc_pages={}",
        if commit_known {
            "known"
        } else {
            "relied/skipped"
        }
    );
    let _ = write!(report, " result={result}");
    // The install line (and the data folder) first; then the writer, which is
    // the only thing that appends after it.
    report_install(&mut report);
    if !start_writer() {
        let mut line = Line::<96>::new();
        let _ = writeln!(line, "{} writer=not_started", tick());
        append_log(line.as_bytes());
    }
}
