//! Chaining onto another plugin's hook that does not use inline-hook sharing
//! v1 (`share`), so both plugins keep working. Same rules as elite-rematch
//! (`model::entry`, `runtime::hook_inline`). From gsp-visualizer 0.3.0's
//! `src/chain.rs` (as in ssbu-quiet-elite-switch), with two changes for
//! memory_guard, which hooks several sites: one stub per site (`STUBS`, the
//! `stub` argument of `hook::inline`), and `near_only` for sites whose next
//! words are a branch target (a far jump there would cover it; the words are
//! put back instead).
//!
//! Skyline writes a hook as a one-word `b`, or as a far jump `ldr x17, #8;
//! br x17; .quad target` (16 bytes), with a `nop` first at an address 4 past
//! an 8-byte boundary (20 bytes). At a site this plugin hooks:
//!
//! - another plugin's far jump: this plugin's hook goes on a small stub
//!   instead, which then jumps on to that plugin's handler through x17 as the
//!   far jump did (Skyline must not relocate the jump's address word as code);
//! - another plugin's one-word branch: Skyline relocates it into this
//!   plugin's trampoline, which works only if this hook is a one-word branch
//!   too (checked; otherwise the words are put back);
//! - other plugins' hooks further into the 64 checked bytes: accepted;
//! - anything else: the site is left alone.
//!
//! Where a plugin only calls a function, or relies on an instruction in
//! place, other plugins' hooks there are accepted the same way.

/// Instructions read at a site: its 64 guard bytes.
pub const WORDS: usize = 16;

/// Skyline's far jump, `ldr x17, #8; br x17`, then the 8-byte target; a
/// `nop` comes first where the target would not be 8-byte aligned.
pub const FAR_JUMP: [u32; 2] = [0x5800_0051, 0xd61f_0220];
pub const NOP: u32 = 0xd503_201f;

/// The guard bytes as instructions.
pub const fn words(guard: &[u8; 64]) -> [u32; WORDS] {
    let mut out = [0; WORDS];
    let mut i = 0;
    while i < WORDS {
        out[i] = u32::from_le_bytes([
            guard[4 * i],
            guard[4 * i + 1],
            guard[4 * i + 2],
            guard[4 * i + 3],
        ]);
        i += 1;
    }
    out
}

/// Words a Skyline far jump at `address` takes (the most a hook replaces).
pub fn hooked_words(address: u64) -> usize {
    if address.is_multiple_of(8) {
        4
    } else {
        5
    }
}

/// A Skyline far jump at the start of `words` (the first one at `address`):
/// its length in words and, when all of it is in `words`, its target.
pub fn far_jump(words: &[u32], address: u64) -> Option<(usize, Option<u64>)> {
    let start = hooked_words(address) - 4;
    if start == 1 && words.first() != Some(&NOP) {
        return None;
    }
    if words.get(start..start + 2)? != FAR_JUMP {
        return None;
    }
    let target = words
        .get(start + 2..start + 4)
        .map(|w| u64::from(w[0]) | (u64::from(w[1]) << 32));
    Some((start + 4, target))
}

/// The target of a `b` at `address`.
pub fn branch(word: u32, address: u64) -> Option<u64> {
    (word & 0xfc00_0000 == 0x1400_0000)
        .then(|| address.wrapping_add_signed(i64::from(((word << 6) as i32) >> 4)))
}

/// A far jump of `len` words at `address` to `target` that could be a hook:
/// to code, and not into the jump itself.
fn jumps_out(target: u64, address: u64, len: usize) -> bool {
    target != 0
        && target.is_multiple_of(4)
        && !(address..address + 4 * len as u64).contains(&target)
}

/// What holds a site this plugin hooks when it installs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Entry {
    /// The 13.0.5 instructions. Other plugins' hooks may sit further on,
    /// past the words this plugin's hook replaces.
    Stock,
    /// Another plugin's far jump over the site, to this handler, which runs
    /// the instructions its jump replaced: this plugin's hook continues into
    /// it.
    Chain(u64),
    /// Another plugin's one-word branch over the first word, to this address.
    Branch(u64),
    /// Anything else: the site is left alone.
    Foreign,
}

impl Entry {
    pub fn name(self) -> &'static str {
        match self {
            Self::Stock => "stock",
            Self::Chain(_) => "chain",
            Self::Branch(_) => "branch",
            Self::Foreign => "foreign",
        }
    }
}

/// Whether `words[i..]` (word `i` at `address + 4·i`) holds only the 13.0.5
/// instructions and other plugins' hooks; the words before `strict` may not
/// change at all.
fn stock_or_hooks(
    stock: &[u32; WORDS],
    words: &[u32; WORDS],
    address: u64,
    mut i: usize,
    strict: usize,
) -> bool {
    while i < WORDS {
        let at = address + 4 * i as u64;
        if words[i] == stock[i] {
            i += 1;
        } else if i < strict {
            return false;
        } else if let Some((len, target)) = far_jump(&words[i..], at) {
            if target.is_some_and(|t| !jumps_out(t, at, len)) {
                return false;
            }
            i += len;
        } else if branch(words[i], at).is_some() {
            i += 1;
        } else {
            return false;
        }
    }
    true
}

/// Reads a site this plugin hooks: its `words` at `address` against the
/// 13.0.5 `stock` ones.
pub fn entry(stock: &[u32; WORDS], words: &[u32; WORDS], address: u64) -> Entry {
    let (entry, i) = match far_jump(words, address) {
        Some((len, Some(target))) if jumps_out(target, address, len) => (Entry::Chain(target), len),
        Some(_) => return Entry::Foreign,
        None => match branch(words[0], address) {
            Some(target) if words[0] != stock[0] => (Entry::Branch(target), 1),
            _ => (Entry::Stock, 0),
        },
    };
    if stock_or_hooks(stock, words, address, i, hooked_words(address)) {
        entry
    } else {
        Entry::Foreign
    }
}

/// Reads a site this plugin calls or relies on in place: the first `kept`
/// instructions must be the 13.0.5 ones; from there on, other plugins' hooks
/// are accepted (calls and the code after them go through those hooks).
pub fn usable(stock: &[u32; WORDS], words: &[u32; WORDS], address: u64, kept: usize) -> bool {
    stock_or_hooks(stock, words, address, 0, kept)
}

/// `foreign=inline|far` in the diagnostic build's `diag.txt` (Eden only):
/// before its own hook the plugin hooks its sites the way a plugin that does
/// not share would (`hook::standin`), so the install meets that hook.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Foreign {
    None,
    /// Inline hooks (Skyline writes one-word branches in Eden).
    Inline,
    /// A far jump on the hooked site (a replace hook to a stand-in handler).
    Far,
}

impl Foreign {
    pub fn parse(text: &[u8]) -> Self {
        text.split(|b| *b == b'\n').fold(Self::None, |found, line| {
            let Some(eq) = line.iter().position(|b| *b == b'=') else {
                return found;
            };
            match (line[..eq].trim_ascii(), line[eq + 1..].trim_ascii()) {
                (b"foreign", b"inline") => Self::Inline,
                (b"foreign", b"far") => Self::Far,
                (b"foreign", _) => Self::None,
                _ => found,
            }
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Inline => "inline",
            Self::Far => "far",
        }
    }
}

/// Installing a hook over whatever `entry` found (Switch only).
#[cfg(target_os = "switch")]
pub mod hook {
    use super::{branch, entry, far_jump, hooked_words, Entry, WORDS};
    use skyline::hooks::{A64HookFunction, A64InlineHook, InlineCtx};
    use skyline::libc::c_void;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Stubs: one per hooked site (memory_guard hooks up to this many).
    pub const STUBS: usize = 10;

    /// Where each stub jumps on to: the other plugin's handler at that site.
    static STUB_TARGETS: [AtomicU64; STUBS] = [const { AtomicU64::new(0) }; STUBS];

    /// Five `nop`s for this plugin's inline hook (all Skyline relocates
    /// there), then a jump to `STUB_TARGETS[index]` through x17.
    macro_rules! stubs {
        ($($index:literal => $name:ident),* $(,)?) => {
            extern "C" {
                $(fn $name();)*
            }
            $(
                core::arch::global_asm!(
                    concat!(".pushsection .text.", stringify!($name), ", \"ax\", %progbits"),
                    ".balign 16",
                    concat!(".hidden ", stringify!($name)),
                    concat!(".global ", stringify!($name)),
                    concat!(stringify!($name), ":"),
                    "nop",
                    "nop",
                    "nop",
                    "nop",
                    "nop",
                    "adrp x17, {targets}",
                    "add x17, x17, #:lo12:{targets}",
                    concat!("ldr x17, [x17, #", stringify!($index), " * 8]"),
                    "br x17",
                    ".popsection",
                    targets = sym STUB_TARGETS,
                );
            )*
            /// The stub for site `index`.
            fn stub_address(index: usize) -> Option<u64> {
                match index {
                    $($index => Some($name as unsafe extern "C" fn() as usize as u64),)*
                    _ => None,
                }
            }
        };
    }

    stubs!(
        0 => ssbu_chain_stub_0,
        1 => ssbu_chain_stub_1,
        2 => ssbu_chain_stub_2,
        3 => ssbu_chain_stub_3,
        4 => ssbu_chain_stub_4,
        5 => ssbu_chain_stub_5,
        6 => ssbu_chain_stub_6,
        7 => ssbu_chain_stub_7,
        8 => ssbu_chain_stub_8,
        9 => ssbu_chain_stub_9,
    );

    /// How a site ended up; the install line shows `<site>=<entry>/<outcome>`.
    #[derive(Clone, Copy, PartialEq, Eq)]
    pub enum Outcome {
        /// Hooked: Skyline wrote a one-word branch there.
        Near,
        /// Hooked: Skyline wrote a far jump there.
        Far,
        /// Left as it was.
        Skipped,
        /// The hook did not check out; the words seen before were put back.
        Restored,
    }

    impl Outcome {
        pub fn name(self) -> &'static str {
            match self {
                Self::Near => "near",
                Self::Far => "far",
                Self::Skipped => "skipped",
                Self::Restored => "restored",
            }
        }

        pub fn hooked(self) -> bool {
            matches!(self, Self::Near | Self::Far)
        }
    }

    unsafe fn read_u32(address: u64) -> u32 {
        (address as *const u32).read_volatile()
    }

    /// The `N` instructions at `address`.
    ///
    /// # Safety
    /// `address` and the `4·N` bytes after it must be mapped readable.
    pub unsafe fn words_at<const N: usize>(address: u64) -> [u32; N] {
        core::array::from_fn(|i| read_u32(address + 4 * i as u64))
    }

    /// One mapping as the kernel reports it (`svcQueryMemory`).
    #[repr(C)]
    #[derive(Default)]
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

    /// Whether `size` bytes at `address` are mapped readable and executable,
    /// checked before chaining to another plugin's handler.
    ///
    /// # Safety
    /// Only asks the kernel (`svcQueryMemory`); any address may be passed.
    pub unsafe fn executable(address: u64, size: u64) -> bool {
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
        result as u32 == 0
            && info.permission & 5 == 5
            && info.address <= address
            && address
                .checked_add(size)
                .is_some_and(|end| end <= info.address.saturating_add(info.size))
    }

    /// The hook now at `address`: how it was written and where it goes.
    fn written(words: &[u32], address: u64) -> Option<(Outcome, u64)> {
        if let Some(target) = branch(words[0], address) {
            return Some((Outcome::Near, target));
        }
        match far_jump(words, address) {
            Some((_, Some(target))) => Some((Outcome::Far, target)),
            _ => None,
        }
    }

    /// Puts back the words a hook at `address` replaced.
    unsafe fn restore(address: u64, before: &[u32; WORDS]) {
        let _ = skyline::patching::sky_memcpy(
            address as *const c_void,
            before.as_ptr().cast(),
            4 * hooked_words(address),
        );
    }

    /// Hooks the inline site at `address` (13.0.5 instructions `stock`) with
    /// `callback`, chaining onto another plugin's hook found there through
    /// stub `stub`. With `near_only`, a hook Skyline wrote as a far jump is
    /// taken out again (`Restored`).
    ///
    /// # Safety
    /// `address` must be an instruction in the game's `.text` with its 64
    /// bytes mapped, `stock` the 13.0.5 words there, and `callback` safe to
    /// run on any thread that executes the site.
    pub unsafe fn inline(
        address: u64,
        stock: &[u32; WORDS],
        callback: unsafe extern "C" fn(&mut InlineCtx),
        stub_index: usize,
        near_only: bool,
    ) -> (Entry, Outcome) {
        let before = words_at::<WORDS>(address);
        let entry = entry(stock, &before, address);
        let callback = callback as *const c_void;
        let Some(stub) = stub_address(stub_index) else {
            return (entry, Outcome::Skipped);
        };
        match entry {
            Entry::Foreign => return (entry, Outcome::Skipped),
            Entry::Chain(target) => {
                if !executable(target, 4) {
                    return (entry, Outcome::Skipped);
                }
                STUB_TARGETS[stub_index].store(target, Ordering::Release);
                A64InlineHook(stub as *const c_void, callback);
                if written(&words_at::<5>(stub), stub).is_none() {
                    return (entry, Outcome::Skipped);
                }
                A64HookFunction(
                    address as *const c_void,
                    stub as *const c_void,
                    core::ptr::null_mut(),
                );
            }
            Entry::Stock | Entry::Branch(_) => A64InlineHook(address as *const c_void, callback),
        }
        let after = words_at::<WORDS>(address);
        let replaced = hooked_words(address);
        let hooked = match (entry, written(&after, address)) {
            (Entry::Stock, Some((outcome, _))) => Some(outcome),
            (Entry::Chain(_), Some((outcome, to))) if to == stub => Some(outcome),
            // The other plugin's handler returns past its one-word branch.
            (Entry::Branch(theirs), Some((Outcome::Near, to)))
                if to != theirs && after[1..replaced] == before[1..replaced] =>
            {
                Some(Outcome::Near)
            }
            _ => None,
        };
        match hooked {
            // A far jump at a near-only site would cover a branch target.
            Some(Outcome::Far) if near_only => {
                restore(address, &before);
                (entry, Outcome::Restored)
            }
            Some(outcome) => (entry, outcome),
            None => {
                restore(address, &before);
                (entry, Outcome::Restored)
            }
        }
    }

    /// Eden-only stand-ins for another plugin that hooks without sharing v1
    /// (`foreign=` in `diag.txt`), installed before this plugin's own hook.
    #[cfg(feature = "diag")]
    pub mod standin {
        use super::*;

        /// Runs of the stand-ins: inline hooks 0 and 1, then the far jump.
        pub static HITS: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];
        /// The trampoline Skyline built for the far jump (the replaced
        /// instructions, then back to the site).
        static ORIGINAL: AtomicU64 = AtomicU64::new(0);

        unsafe extern "C" fn inline_0(_: &mut InlineCtx) {
            HITS[0].fetch_add(1, Ordering::Relaxed);
        }

        unsafe extern "C" fn inline_1(_: &mut InlineCtx) {
            HITS[1].fetch_add(1, Ordering::Relaxed);
        }

        extern "C" {
            /// Counts a run in `HITS[2]`, then jumps to `ORIGINAL`. Keeps every
            /// register but x17, as Skyline's far jump does.
            fn ssbu_chain_standin();
        }

        core::arch::global_asm!(
            ".pushsection .text.ssbu_chain_standin, \"ax\", %progbits",
            ".balign 16",
            ".hidden ssbu_chain_standin",
            ".global ssbu_chain_standin",
            "ssbu_chain_standin:",
            "stp x16, x17, [sp, #-16]!",
            "adrp x16, {hits}",
            "add x16, x16, #:lo12:{hits}",
            "ldr x17, [x16, #16]",
            "add x17, x17, #1",
            "str x17, [x16, #16]",
            "ldp x16, x17, [sp], #16",
            "adrp x17, {original}",
            "ldr x17, [x17, #:lo12:{original}]",
            "br x17",
            ".popsection",
            hits = sym HITS,
            original = sym ORIGINAL,
        );

        /// An inline hook at `address`, as a plugin's `#[skyline::hook(inline)]`.
        ///
        /// # Safety
        /// Eden only: `address` must be an instruction in the game's `.text`.
        pub unsafe fn inline(address: u64, which: usize) {
            let callback: unsafe extern "C" fn(&mut InlineCtx) =
                if which == 0 { inline_0 } else { inline_1 };
            A64InlineHook(address as *const c_void, callback as *const c_void);
        }

        /// A far jump at `address` to a handler that runs the replaced
        /// instructions (Skyline's replace hook writes it as a far jump).
        ///
        /// # Safety
        /// Eden only: `address` must be an instruction in the game's `.text`,
        /// and only one site may get this stand-in.
        pub unsafe fn far(address: u64) {
            A64HookFunction(
                address as *const c_void,
                ssbu_chain_standin as *const c_void,
                ORIGINAL.as_ptr().cast(),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in for 16 instructions of game code (no branches).
    const STOCK: [u32; WORDS] = {
        let mut out = [0; WORDS];
        let mut i = 0;
        while i < WORDS {
            out[i] = 0xaa00_03e0 | i as u32; // mov x0, x<i>
            i += 1;
        }
        out
    };
    const AT8: u64 = 0x80_01a1_c118; // 8-byte aligned, like the hooked sites
    const AT4: u64 = 0x80_01a1_9c44; // 4 past, like the hover call
    const HANDLER: u64 = 0x71_2345_6780;

    fn far(target: u64) -> [u32; 4] {
        [
            FAR_JUMP[0],
            FAR_JUMP[1],
            target as u32,
            (target >> 32) as u32,
        ]
    }

    /// `b` from `from` to `to`.
    fn b(from: u64, to: u64) -> u32 {
        0x1400_0000 | ((to.wrapping_sub(from) >> 2) as u32 & 0x03ff_ffff)
    }

    #[test]
    fn foreign_setting_parse() {
        assert_eq!(Foreign::parse(b""), Foreign::None);
        assert_eq!(
            Foreign::parse(b"dot=dash\n foreign = far \r\n"),
            Foreign::Far
        );
        assert_eq!(Foreign::parse(b"foreign=inline"), Foreign::Inline);
        assert_eq!(Foreign::parse(b"foreign=far\nforeign=0"), Foreign::None);
    }

    #[test]
    fn guard_bytes_become_words() {
        let mut guard = [0u8; 64];
        guard[..4].copy_from_slice(&[0xe0, 0x07, 0x40, 0xf9]);
        assert_eq!(words(&guard)[0], 0xf940_07e0); // ldr x0, [sp, #8]
    }

    #[test]
    fn stock_sites_are_stock() {
        assert_eq!(entry(&STOCK, &STOCK, AT8), Entry::Stock);
        assert_eq!(entry(&STOCK, &STOCK, AT4), Entry::Stock);
        assert!(usable(&STOCK, &STOCK, AT4, 1));
    }

    #[test]
    fn another_plugins_far_jump_is_chained() {
        let mut words = STOCK;
        words[..4].copy_from_slice(&far(HANDLER));
        assert_eq!(entry(&STOCK, &words, AT8), Entry::Chain(HANDLER));
        // Not 8-byte aligned: Skyline puts a nop first.
        let mut words = STOCK;
        words[0] = NOP;
        words[1..5].copy_from_slice(&far(HANDLER));
        assert_eq!(entry(&STOCK, &words, AT4), Entry::Chain(HANDLER));
        // That form without its nop is not Skyline's.
        let mut words = STOCK;
        words[..4].copy_from_slice(&far(HANDLER));
        assert_eq!(entry(&STOCK, &words, AT4), Entry::Foreign);
    }

    #[test]
    fn far_jumps_to_nowhere_or_into_themselves_are_foreign() {
        for target in [0, HANDLER + 2, AT8, AT8 + 8, AT8 + 12] {
            let mut words = STOCK;
            words[..4].copy_from_slice(&far(target));
            assert_eq!(entry(&STOCK, &words, AT8), Entry::Foreign, "{target:#x}");
            assert!(!usable(&STOCK, &words, AT8, 0), "{target:#x}");
        }
        let mut words = STOCK;
        words[..4].copy_from_slice(&far(AT8 + 16));
        assert_eq!(entry(&STOCK, &words, AT8), Entry::Chain(AT8 + 16));
    }

    #[test]
    fn another_plugins_branch_is_found_both_ways() {
        for to in [AT8 + 0x40_0000, AT8 - 0x40_0000] {
            let mut words = STOCK;
            words[0] = b(AT8, to);
            assert_eq!(branch(words[0], AT8), Some(to));
            assert_eq!(entry(&STOCK, &words, AT8), Entry::Branch(to));
        }
    }

    #[test]
    fn changes_in_the_replaced_words_are_foreign() {
        for i in 0..hooked_words(AT8) {
            let mut words = STOCK;
            words[i] ^= 0x10;
            assert_eq!(entry(&STOCK, &words, AT8), Entry::Foreign, "word {i}");
        }
        // A far jump starting inside them.
        let mut words = STOCK;
        words[2..6].copy_from_slice(&far(HANDLER));
        assert_eq!(entry(&STOCK, &words, AT8), Entry::Foreign);
        // A branch over the first word, then something else changed.
        let mut words = STOCK;
        words[0] = b(AT8, HANDLER);
        words[2] ^= 0x10;
        assert_eq!(entry(&STOCK, &words, AT8), Entry::Foreign);
    }

    #[test]
    fn other_hooks_past_the_replaced_words_are_accepted() {
        let mut words = STOCK;
        words[6..10].copy_from_slice(&far(HANDLER));
        words[11] = b(AT8 + 44, HANDLER);
        words[14..16].copy_from_slice(&FAR_JUMP); // its target is past the window
        assert_eq!(entry(&STOCK, &words, AT8), Entry::Stock);
        words[..4].copy_from_slice(&far(HANDLER));
        assert_eq!(entry(&STOCK, &words, AT8), Entry::Chain(HANDLER));
        // Anything that is not a hook is not.
        words[13] ^= 0x10;
        assert_eq!(entry(&STOCK, &words, AT8), Entry::Foreign);
    }

    #[test]
    fn called_functions_accept_hooks_anywhere() {
        // An entry hook either way, plus one further in.
        let mut words = STOCK;
        words[..4].copy_from_slice(&far(HANDLER));
        words[8] = b(AT8 + 32, HANDLER);
        assert!(usable(&STOCK, &words, AT8, 0));
        let mut words = STOCK;
        words[0] = b(AT8, HANDLER);
        assert!(usable(&STOCK, &words, AT8, 0));
        // Anything that is not a hook is not.
        words[5] ^= 0x10;
        assert!(!usable(&STOCK, &words, AT8, 0));
    }

    #[test]
    fn relied_on_instructions_must_stay() {
        // The hover call's own word is kept; a hook right after it is fine.
        let mut words = STOCK;
        words[1..5].copy_from_slice(&far(HANDLER)); // AT4 + 4 is 8-byte aligned
        assert!(usable(&STOCK, &words, AT4, 1));
        let mut words = STOCK;
        words[0] = b(AT4, HANDLER);
        assert!(!usable(&STOCK, &words, AT4, 1));
        let mut words = STOCK;
        words[0] = NOP;
        words[1..5].copy_from_slice(&far(HANDLER));
        assert!(!usable(&STOCK, &words, AT4, 1));
    }
}
