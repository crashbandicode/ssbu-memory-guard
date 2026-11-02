//! Host-testable half: the 13.0.5 sites, the one rule for what a value may be
//! changed to, and the text of every line the plugin writes.
//!
//! The crashes behind each site are Atmosphère reports from the operator's
//! card (Smash 13.0.5, read 2026-11-02). In each, a pointer the game read
//! held one bit that should have been zero:
//!
//! | Site | Report | Value |
//! | --- | --- | --- |
//! | `dispatch_list` | 2026-11-02 15:02, render dispatch | `0x6ad817c790` with bit 42 |
//! | `component_get` | 2026-10-06 12:08, TaskWorker2 | empty, with bit 42 |
//! | `list_head`, `list_next` | 2026-10-15 11:22, TaskWorker2 | empty, with bit 41 |
//! | `field_a8` | 2026-10-06 12:37, MainThread | empty, with bit 25 |
//! | `rehash_head`, `rehash_next` | 2026-10-23 15:24, TaskWorker2 | empty, with bit 28 |
//!
//! In every case the game was about to read through the pointer (after an
//! "is it empty?" check the stray bit passed) and faulted. Each hook sits on
//! the instruction that loads the pointer and checks the memory it loads from
//! first; the rule changes only a value the game would fault on, and only to
//! what it was before the bit appeared:
//!
//! - one bit above the 39-bit address space over a mapped address: that
//!   address (the bit cleared);
//! - one bit above the address space over nothing, or one lone bit inside it
//!   (at least `0x10000`) where nothing is mapped: empty, so the game takes the
//!   path it takes for an empty pointer.
//!
//! Anything else is left as it is. A value the game can read through is never
//! changed.
//!
//! No hook sits on a PC-relative instruction, and every hook must come out as
//! a one-word branch: in Eden (q01, 2026-11-02) a taken `cbnz` that Skyline's
//! inline hook had moved into its trampoline jumped 0x200000 past its target,
//! and a far jump would move the PC-relative instructions after a site too.
//!
//! `abort_stub` covers the other kind of startup crash: the game's heap page
//! provider (`main+0x37c9600`) gives pages back with `nn::os::FreeMemoryPages`
//! and calls `abort()` when the kernel refuses (12 reports, 2026-10-13 to
//! 2026-11-02). The hook in `abort`'s import stub records the refusal and the
//! kernel's view of the pages (or, for any other caller, where `abort` was
//! called from), then lets the abort go on.

use crate::chain;
use crate::guards;
use core::fmt::Write;

pub const SUPPORTED_VERSION: &str = "13.0.5";

/// User addresses on this console fit in 39 bits.
pub const ADDRESS_BITS: u32 = 39;
pub const ADDRESS_MASK: u64 = (1 << ADDRESS_BITS) - 1;
/// A lone bit below this is a small number, not a stray bit on an empty pointer.
pub const LONE_BIT_MIN: u64 = 1 << 16;
/// Bytes that must be readable at a repaired pointer (the reads the game makes
/// next stay inside them: `+0x8`, `+0xc`, `+0x10`).
pub const REPAIR_SPAN: u64 = 0x18;

/// Where `abort` returns to after the refused page release (`bl abort` at
/// `main+0x37caeec`).
pub const ABORT_AFTER_RELEASE: usize = 0x37c_aef0;

/// What a value read at a site is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Readable as it is, or not a pointer this rule knows: unchanged.
    Keep,
    /// One stray bit above the address space; this is the address under it.
    Repair(u64),
    /// An empty pointer with one stray bit; empty is what it was.
    Empty,
    /// Above the address space but not one stray bit over a mapped address:
    /// the game will fault on it, and nothing here knows the right value.
    Unfixable,
}

impl Verdict {
    pub fn name(self) -> &'static str {
        match self {
            Self::Keep => "keep",
            Self::Repair(_) => "repair",
            Self::Empty => "empty",
            Self::Unfixable => "unfixable",
        }
    }

    /// The value the game should read instead, when there is one.
    pub fn replacement(self) -> Option<u64> {
        match self {
            Self::Repair(to) => Some(to),
            Self::Empty => Some(0),
            Self::Keep | Self::Unfixable => None,
        }
    }
}

/// The cheap first look every hook makes: `false` for every ordinary pointer,
/// empty pointer and small number, so the common path never asks the kernel.
pub const fn suspicious(value: u64) -> bool {
    value >> ADDRESS_BITS != 0 || (value >= LONE_BIT_MIN && value & (value - 1) == 0)
}

/// The rule. `mapped(address)` answers whether `REPAIR_SPAN` bytes at
/// `address` can be read (the kernel's `svcQueryMemory` on the console).
pub fn classify(value: u64, mapped: impl Fn(u64) -> bool) -> Verdict {
    if !suspicious(value) {
        return Verdict::Keep;
    }
    let high = value >> ADDRESS_BITS;
    if high != 0 {
        if high.count_ones() != 1 {
            return Verdict::Unfixable;
        }
        let low = value & ADDRESS_MASK;
        if low == 0 {
            return Verdict::Empty;
        }
        return if mapped(low) {
            Verdict::Repair(low)
        } else {
            Verdict::Unfixable
        };
    }
    // One lone bit inside the address space: an address only when something
    // is mapped there.
    if mapped(value) {
        Verdict::Keep
    } else {
        Verdict::Empty
    }
}

/// What a site's hook does with the registers it sees.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// `x[begin]..x[end]` is the frame's list of command-buffer pointers (each
    /// read at `+0x8` next); entries are checked and repaired in place. An
    /// entry cannot be emptied, so `Empty` counts as `Unfixable` here.
    PointerList { begin: u8, end: u8 },
    /// The hooked instruction loads a pointer from `x[base] + x[index] +
    /// offset`: that slot is checked, and repaired in place, before the load.
    LoadSlot {
        base: u8,
        index: Option<u8>,
        offset: u16,
    },
    /// The second instruction of `abort`'s import stub: `x30` says who called.
    /// From the refused release (`ABORT_AFTER_RELEASE`): `w0` the result,
    /// `x20` the address, `x21` the size.
    AbortStub,
    /// Diag only: just before the release call (`mov x1, x21`, with `x0`
    /// already the address); the forced refusal passes address 0 instead.
    ForceRelease,
}

/// A window of instructions a hook relies on: the first `kept` must be
/// exactly the 13.0.5 ones; after them other plugins' hooks are accepted.
pub struct Relied {
    pub offset: usize,
    pub bytes: [u8; 64],
    pub kept: usize,
}

impl Relied {
    pub fn stock(&self) -> [u32; chain::WORDS] {
        chain::words(&self.bytes)
    }
}

/// A hooked site and the 64 bytes there in 13.0.5, plus the windows of
/// instructions the hook relies on (the loads and calls that give a register
/// its meaning).
pub struct Site {
    pub name: &'static str,
    pub offset: usize,
    pub guard: [u8; 64],
    pub relied: &'static [Relied],
    pub action: Action,
    /// Installed only in the diagnostic build (and only when its setting asks).
    pub diag_only: bool,
}

impl Site {
    /// The 13.0.5 instructions at the site.
    pub fn stock(&self) -> [u32; chain::WORDS] {
        chain::words(&self.guard)
    }
}

const ABORT_RELIED: [Relied; 3] = [
    // The stub itself, from its adrp (the site is its second word).
    Relied {
        offset: 0x39c_2650,
        bytes: guards::ABORT_STUB_RELIED,
        kept: 1,
    },
    // `bl abort` after the refused release, so its return address means that.
    Relied {
        offset: 0x37c_aeec,
        bytes: guards::ABORT_CALL_RELIED,
        kept: 1,
    },
    // mov w8, w21; lsl x21, x8, #16; mov x0, x20; mov x1, x21; bl FreeMemoryPages
    // (x20 the address, x21 the size); the refusal check after it may be hooked.
    Relied {
        offset: 0x37c_ae10,
        bytes: guards::FREE_PAGES_RELIED,
        kept: 5,
    },
];

/// Every site, in install order. Indices are the hook and stub numbers.
pub const SITES: [Site; 9] = [
    Site {
        name: "dispatch_list",
        offset: 0x381_6814,
        guard: guards::DISPATCH_LIST,
        // ldp x26, x27, [sp, #0x10] ... up to the site.
        relied: &[Relied {
            offset: 0x381_67e8,
            bytes: guards::DISPATCH_LIST_RELIED,
            kept: 11,
        }],
        action: Action::PointerList { begin: 26, end: 27 },
        diag_only: false,
    },
    Site {
        name: "component_get",
        offset: 0x3_810c,
        guard: guards::COMPONENT_GET,
        // adrp; add; ldr x8, [x8, w1, uxtw #3]: x8 is the slot's offset.
        relied: &[Relied {
            offset: 0x3_8100,
            bytes: guards::COMPONENT_GET_RELIED,
            kept: 3,
        }],
        action: Action::LoadSlot {
            base: 0,
            index: Some(8),
            offset: 0,
        },
        diag_only: false,
    },
    Site {
        name: "list_head",
        offset: 0x377_83b0,
        guard: guards::LIST_HEAD,
        relied: &[],
        action: Action::LoadSlot {
            base: 21,
            index: None,
            offset: 8,
        },
        diag_only: false,
    },
    Site {
        name: "list_next",
        offset: 0x377_83bc,
        guard: guards::LIST_NEXT,
        relied: &[],
        action: Action::LoadSlot {
            base: 8,
            index: None,
            offset: 0,
        },
        diag_only: false,
    },
    Site {
        name: "field_a8",
        offset: 0x5_9860,
        guard: guards::FIELD_A8,
        relied: &[],
        action: Action::LoadSlot {
            base: 0,
            index: None,
            offset: 0xa8,
        },
        diag_only: false,
    },
    Site {
        name: "rehash_head",
        offset: 0x391_756c,
        guard: guards::REHASH_HEAD,
        relied: &[],
        action: Action::LoadSlot {
            base: 11,
            index: Some(12),
            offset: 0,
        },
        diag_only: false,
    },
    Site {
        name: "rehash_next",
        offset: 0x391_758c,
        guard: guards::REHASH_NEXT,
        relied: &[],
        action: Action::LoadSlot {
            base: 9,
            index: None,
            offset: 0x10,
        },
        diag_only: false,
    },
    Site {
        name: "abort_stub",
        offset: 0x39c_2654,
        guard: guards::ABORT_STUB,
        relied: &ABORT_RELIED,
        action: Action::AbortStub,
        diag_only: false,
    },
    Site {
        name: "free_pages_force",
        offset: 0x37c_ae1c,
        guard: guards::FREE_PAGES_FORCE,
        relied: &[Relied {
            offset: 0x37c_ae10,
            bytes: guards::FREE_PAGES_RELIED,
            kept: 3,
        }],
        action: Action::ForceRelease,
        diag_only: true,
    },
];

/// The site table index of a name (diag settings name sites).
pub fn site_index(name: &[u8]) -> Option<usize> {
    SITES.iter().position(|site| site.name.as_bytes() == name)
}

/// Whether `word` reads or jumps relative to its own address (B, BL, B.cond,
/// CBZ/CBNZ, TBZ/TBNZ, ADR/ADRP, LDR literal, PRFM literal): such an
/// instruction must not be hooked (see the module notes).
pub const fn pc_relative(word: u32) -> bool {
    word & 0x7c00_0000 == 0x1400_0000 // B, BL
        || word & 0xff00_0010 == 0x5400_0000 // B.cond
        || word & 0x7e00_0000 == 0x3400_0000 // CBZ, CBNZ
        || word & 0x7e00_0000 == 0x3600_0000 // TBZ, TBNZ
        || word & 0x1f00_0000 == 0x1000_0000 // ADR, ADRP
        || word & 0x3b00_0000 == 0x1800_0000 // LDR (literal), LDRSW/PRFM literal
}

/// A Horizon result as the console shows it: `2000 + module`, then the description.
pub const fn result_code(result: u32) -> (u32, u32) {
    (2000 + (result & 0x1ff), (result >> 9) & 0x1fff)
}

/// One memory block the kernel reports for an address (`svcQueryMemory`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Block {
    pub address: u64,
    pub size: u64,
    pub state: u32,
    pub attributes: u32,
    pub permission: u32,
    pub ipc_references: u32,
    pub device_references: u32,
}

/// Blocks recorded for a refused page release (the range is usually one block).
pub const FREE_BLOCKS: usize = 6;

/// What the plugin knows when the kernel refused to take pages back.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FreeFailure {
    pub result: u32,
    pub address: u64,
    pub size: u64,
    /// Process memory, `svcGetInfo` 6 (total) and 7 (used); `u64::MAX` when unread.
    pub total: u64,
    pub used: u64,
    pub blocks: [Block; FREE_BLOCKS],
    pub block_count: usize,
    /// Set by the diagnostic build's forced refusal (Eden); the console never sets it.
    pub forced: bool,
}

/// One planted stray bit (diag): on an empty pointer or a real one, which bit,
/// and how many eligible calls to let pass first (to plant after boot).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Injection {
    pub empty: bool,
    pub bit: u32,
    pub after: u64,
}

impl Injection {
    /// `mode[:bit[:after]]` after the site name; defaults `pointer`, 42, 0.
    fn parse(fields: &mut core::slice::Split<'_, u8, impl FnMut(&u8) -> bool>) -> Option<Self> {
        let mut out = Self {
            empty: false,
            bit: 42,
            after: 0,
        };
        if let Some(mode) = fields.next() {
            out.empty = match mode.trim_ascii() {
                b"empty" => true,
                b"pointer" | b"" => false,
                _ => return None,
            };
        }
        if let Some(bit) = fields.next() {
            out.bit = parse_u64(bit.trim_ascii()).filter(|b| (16..64).contains(b))? as u32;
        }
        if let Some(after) = fields.next() {
            out.after = parse_u64(after.trim_ascii())?;
        }
        Some(out)
    }
}

/// Diagnostic settings (`diag.txt`, Eden only).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DiagSettings {
    /// Per site: plant a stray bit once
    /// (`inject=<site>[:empty|pointer[:<bit>[:<after>]]],...`).
    pub inject: [Option<Injection>; SITES.len()],
    /// `repair=off`: let the planted values through (the control run).
    pub repair_off: bool,
    /// Refuse the release that makes this count (`free_pages_force`).
    pub force_free_failure: Option<u64>,
}

impl DiagSettings {
    pub fn parse(text: &[u8]) -> Self {
        let mut out = Self::default();
        for line in text.split(|b| *b == b'\n') {
            let Some(eq) = line.iter().position(|b| *b == b'=') else {
                continue;
            };
            let (key, value) = (line[..eq].trim_ascii(), line[eq + 1..].trim_ascii());
            match key {
                b"inject" => {
                    for entry in value.split(|b| *b == b',') {
                        let mut fields = entry.split(|b| *b == b':');
                        let Some(site) = fields.next().and_then(|n| site_index(n.trim_ascii()))
                        else {
                            continue;
                        };
                        out.inject[site] = Injection::parse(&mut fields);
                    }
                }
                b"repair" => out.repair_off = value == b"off",
                b"force_free_failure" => out.force_free_failure = parse_u64(value),
                _ => {}
            }
        }
        out
    }
}

fn parse_u64(text: &[u8]) -> Option<u64> {
    let text = core::str::from_utf8(text).ok()?;
    match text.strip_prefix("0x") {
        Some(hex) => u64::from_str_radix(hex, 16).ok(),
        None => text.parse().ok(),
    }
}

/// `<tick> guard site=<name> verdict=<v> value=<hex> to=<hex> slot=<hex>`: one
/// value the rule acted on (or could not). `applied` is false only in the
/// diagnostic build's control run (`repair=off`).
pub fn event_line(
    out: &mut impl Write,
    tick: u64,
    site: usize,
    verdict: Verdict,
    value: u64,
    slot: u64,
    applied: bool,
) -> core::fmt::Result {
    let name = SITES.get(site).map_or("?", |s| s.name);
    write!(
        out,
        "{tick} guard site={name} verdict={} value={value:#x}",
        verdict.name()
    )?;
    if let Some(to) = verdict.replacement() {
        write!(out, " to={to:#x}")?;
    }
    if slot != 0 {
        write!(out, " slot={slot:#x}")?;
    }
    if !applied {
        write!(out, " applied=0")?;
    }
    writeln!(out)
}

/// `<tick> abort caller=main+<hex>`: `abort` called from somewhere other than
/// the refused release (`caller` is the return address's offset, or the
/// address itself when it is outside the game's code).
pub fn abort_line(
    out: &mut impl Write,
    tick: u64,
    caller: u64,
    in_main: bool,
) -> core::fmt::Result {
    if in_main {
        writeln!(out, "{tick} abort caller=main+{caller:#x}")
    } else {
        writeln!(out, "{tick} abort caller={caller:#x}")
    }
}

/// Verdicts as the hooks pass them to the writer thread (one word each).
pub const fn verdict_code(verdict: Verdict) -> (u64, u64) {
    match verdict {
        Verdict::Keep => (0, 0),
        Verdict::Repair(to) => (1, to),
        Verdict::Empty => (2, 0),
        Verdict::Unfixable => (3, 0),
    }
}

pub const fn verdict_from_code(code: u64, to: u64) -> Verdict {
    match code {
        1 => Verdict::Repair(to),
        2 => Verdict::Empty,
        3 => Verdict::Unfixable,
        _ => Verdict::Keep,
    }
}

/// The refused release: one summary line and one line per block.
pub fn free_lines(out: &mut impl Write, tick: u64, failure: &FreeFailure) -> core::fmt::Result {
    let (module, description) = result_code(failure.result);
    write!(
        out,
        "{tick} free_pages_refused result={:#x} ({module}-{description:04}) address={:#x} size={:#x}",
        failure.result, failure.address, failure.size
    )?;
    if failure.total != u64::MAX {
        write!(out, " total={:#x} used={:#x}", failure.total, failure.used)?;
    }
    if failure.forced {
        write!(out, " forced=1")?;
    }
    writeln!(out)?;
    for (i, block) in failure.blocks[..failure.block_count.min(FREE_BLOCKS)]
        .iter()
        .enumerate()
    {
        writeln!(
            out,
            "{tick} free_pages_block i={i} address={:#x} size={:#x} state={:#x} attributes={:#x} permission={:#x} ipc_references={} device_references={}",
            block.address,
            block.size,
            block.state,
            block.attributes,
            block.permission,
            block.ipc_references,
            block.device_references
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A mapped region for the tests: one heap range.
    fn heap(address: u64) -> bool {
        (0x6a_d800_0000..0x6b_0200_0000).contains(&address)
    }

    #[test]
    fn ordinary_values_are_never_suspicious() {
        for value in [
            0,
            1,
            8,
            0x10,
            0xffff,
            0x6a_d817_c790,
            0x7f_ffff_fff0,
            0x36_afbd_c320,
        ] {
            assert!(!suspicious(value), "{value:#x}");
            assert_eq!(
                classify(value, |_| panic!("asked the kernel")),
                Verdict::Keep
            );
        }
    }

    #[test]
    fn the_crash_values_from_the_card() {
        // 2026-11-02 render dispatch: a real pointer with bit 42.
        assert_eq!(
            classify(0x46a_d817_c790, heap),
            Verdict::Repair(0x6a_d817_c790)
        );
        // 2026-10-06 TaskWorker2: empty with bit 42.
        assert_eq!(classify(1 << 42, heap), Verdict::Empty);
        // 2026-10-15: empty with bit 41.
        assert_eq!(classify(1 << 41, heap), Verdict::Empty);
        // 2026-10-06 MainThread and 2026-10-23: lone bits 25 and 28, nothing mapped.
        assert_eq!(classify(1 << 25, heap), Verdict::Empty);
        assert_eq!(classify(1 << 28, heap), Verdict::Empty);
    }

    #[test]
    fn a_lone_bit_that_is_a_mapped_address_is_kept() {
        let mapped = |a: u64| a == 1 << 38;
        assert_eq!(classify(1 << 38, mapped), Verdict::Keep);
        assert_eq!(classify(1 << 38, |_| false), Verdict::Empty);
    }

    #[test]
    fn small_lone_bits_are_numbers() {
        for bit in 0..16 {
            assert_eq!(classify(1 << bit, |_| false), Verdict::Keep, "bit {bit}");
        }
    }

    #[test]
    fn more_than_one_stray_bit_or_nothing_mapped_is_unfixable() {
        assert_eq!(
            classify(0x46a_d817_c790 | 1 << 50, heap),
            Verdict::Unfixable
        );
        assert_eq!(classify(0x400_1234_5678, heap), Verdict::Unfixable);
        assert_eq!(classify(u64::MAX, heap), Verdict::Unfixable);
    }

    #[test]
    fn verdict_replacements() {
        assert_eq!(Verdict::Repair(5).replacement(), Some(5));
        assert_eq!(Verdict::Empty.replacement(), Some(0));
        assert_eq!(Verdict::Keep.replacement(), None);
        assert_eq!(Verdict::Unfixable.replacement(), None);
    }

    fn by_name(name: &str) -> &'static Site {
        &SITES[site_index(name.as_bytes()).unwrap()]
    }

    /// `ldr Xt, [Xn, #imm]` (64-bit, unsigned offset).
    const fn ldr_imm(rt: u32, rn: u32, imm: u32) -> u32 {
        0xf940_0000 | ((imm / 8) << 10) | (rn << 5) | rt
    }

    /// `ldr Xt, [Xn, Xm]`.
    const fn ldr_reg(rt: u32, rn: u32, rm: u32) -> u32 {
        0xf860_6800 | (rm << 16) | (rn << 5) | rt
    }

    #[test]
    fn each_site_holds_the_instruction_its_action_expects() {
        let site_word = |n: &str| by_name(n).stock()[0];
        assert_eq!(site_word("dispatch_list"), 0xeb1b_035f, "cmp x26, x27");
        assert_eq!(
            by_name("dispatch_list").relied[0].stock()[0],
            0xa941_6ffa,
            "ldp x26, x27, [sp, #0x10]"
        );
        assert_eq!(site_word("component_get"), ldr_reg(0, 0, 8));
        assert_eq!(
            by_name("component_get").relied[0].stock()[2],
            0xf861_5908,
            "ldr x8, [x8, w1, uxtw #3]"
        );
        assert_eq!(site_word("list_head"), ldr_imm(8, 21, 8));
        assert_eq!(site_word("list_next"), ldr_imm(8, 8, 0));
        assert_eq!(site_word("field_a8"), ldr_imm(8, 0, 0xa8));
        assert_eq!(site_word("rehash_head"), ldr_reg(9, 11, 12));
        assert_eq!(site_word("rehash_next"), ldr_imm(13, 9, 0x10));
        assert_eq!(site_word("abort_stub"), ldr_imm(17, 16, 0x208));
        assert_eq!(site_word("free_pages_force"), 0xaa15_03e1, "mov x1, x21");
    }

    #[test]
    fn load_slot_sites_match_their_load() {
        for site in &SITES {
            let Action::LoadSlot {
                base,
                index,
                offset,
            } = site.action
            else {
                continue;
            };
            let expect = match index {
                Some(index) => {
                    assert_eq!(offset, 0, "{}", site.name);
                    ldr_reg(site.stock()[0] & 31, u32::from(base), u32::from(index))
                }
                None => ldr_imm(site.stock()[0] & 31, u32::from(base), u32::from(offset)),
            };
            assert_eq!(site.stock()[0], expect, "{}", site.name);
        }
    }

    #[test]
    fn no_site_is_a_pc_relative_instruction() {
        for site in &SITES {
            assert!(!pc_relative(site.stock()[0]), "{}", site.name);
        }
        // The classes it refuses, from the 13.0.5 code this plugin met.
        for word in [
            0xb500_00c8u32, // cbnz x8
            0xb400_0189,    // cbz x9
            0x5400_0180,    // b.eq
            0x9407_ddd9,    // bl
            0x1400_0001,    // b
            0xd000_c6f0,    // adrp
            0x3500_0640,    // cbnz w0
            0x3600_0000,    // tbz
            0x5800_0051,    // ldr x17, literal
        ] {
            assert!(pc_relative(word), "{word:#x}");
        }
    }

    #[test]
    fn the_abort_hook_relies_on_the_release_and_the_call() {
        let abort = by_name("abort_stub");
        assert_eq!(abort.relied.len(), 3);
        assert_eq!(abort.relied[0].stock()[0], 0xd000_c6f0, "adrp x16");
        assert_eq!(abort.relied[0].stock()[1], abort.stock()[0]);
        assert_eq!(abort.relied[1].stock()[0] >> 26, 0b100101, "bl abort");
        // bl at 0x37caeec targets the stub at 0x39c2650.
        let bl = abort.relied[1].stock()[0];
        let target = abort.relied[1].offset as i64 + ((((bl << 6) as i32) >> 4) as i64);
        assert_eq!(target, 0x39c_2650);
        assert_eq!(ABORT_AFTER_RELEASE, abort.relied[1].offset + 4);
        let release = &abort.relied[2].stock();
        assert_eq!(release[0], 0x2a15_03e8, "mov w8, w21");
        assert_eq!(release[1], 0xd370_bd15, "lsl x21, x8, #16");
        assert_eq!(release[2], 0xaa14_03e0, "mov x0, x20");
        assert_eq!(release[3], 0xaa15_03e1, "mov x1, x21");
        assert_eq!(release[4] >> 26, 0b100101, "bl FreeMemoryPages");
    }

    #[test]
    fn relied_windows_keep_only_what_comes_before_a_site() {
        for site in &SITES {
            for relied in site.relied {
                assert!(relied.kept <= chain::WORDS, "{}", site.name);
                let holds_site = (relied.offset..relied.offset + 64).contains(&site.offset);
                if holds_site {
                    let at = (site.offset - relied.offset) / 4;
                    assert_eq!(relied.kept, at, "{}", site.name);
                    assert_eq!(relied.stock()[at], site.stock()[0], "{}", site.name);
                }
            }
        }
    }

    #[test]
    fn names_are_unique_and_found() {
        for (i, site) in SITES.iter().enumerate() {
            assert_eq!(site_index(site.name.as_bytes()), Some(i));
        }
        assert_eq!(site_index(b"nope"), None);
        assert_eq!(SITES.iter().filter(|s| s.diag_only).count(), 1);
    }

    #[test]
    fn result_codes_read_like_the_console() {
        assert_eq!(result_code(0xd401), (2001, 106));
        assert_eq!(result_code(0xca8), (2168, 6));
    }

    #[test]
    fn diag_settings() {
        let s = DiagSettings::parse(
            b"inject=dispatch_list, list_next:empty:41:100 ,field_a8:empty:25,rehash_head:bogus\nrepair=off\n",
        );
        let at = |n: &[u8]| s.inject[site_index(n).unwrap()];
        assert_eq!(
            at(b"dispatch_list"),
            Some(Injection {
                empty: false,
                bit: 42,
                after: 0
            })
        );
        assert_eq!(
            at(b"list_next"),
            Some(Injection {
                empty: true,
                bit: 41,
                after: 100
            })
        );
        assert_eq!(at(b"field_a8").map(|i| (i.empty, i.bit)), Some((true, 25)));
        assert_eq!(at(b"rehash_head"), None, "unknown mode");
        assert_eq!(at(b"component_get"), None);
        assert!(s.repair_off);
        let s = DiagSettings::parse(b" force_free_failure = 40 \r\ninject=list_head:empty:3\n");
        assert_eq!(s.force_free_failure, Some(40));
        assert_eq!(
            s.inject[site_index(b"list_head").unwrap()],
            None,
            "bits below 16 are refused"
        );
        assert!(DiagSettings::parse(b"inject=nope")
            .inject
            .iter()
            .all(Option::is_none));
    }

    struct Text(String);
    impl Write for Text {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            self.0.push_str(s);
            Ok(())
        }
    }

    #[test]
    fn event_lines() {
        let mut t = Text(String::new());
        event_line(
            &mut t,
            7,
            0,
            Verdict::Repair(0x6a_d817_c790),
            0x46a_d817_c790,
            0x6b_016d_2408,
            true,
        )
        .unwrap();
        assert_eq!(
            t.0,
            "7 guard site=dispatch_list verdict=repair value=0x46ad817c790 to=0x6ad817c790 slot=0x6b016d2408\n"
        );
        let mut t = Text(String::new());
        event_line(&mut t, 8, 5, Verdict::Unfixable, 0x400_1234_5678, 0, true).unwrap();
        assert_eq!(
            t.0,
            "8 guard site=rehash_head verdict=unfixable value=0x40012345678\n"
        );
        let mut t = Text(String::new());
        event_line(&mut t, 9, 3, Verdict::Empty, 1 << 41, 0x10, false).unwrap();
        assert_eq!(
            t.0,
            "9 guard site=list_next verdict=empty value=0x20000000000 to=0x0 slot=0x10 applied=0\n"
        );
        let mut t = Text(String::new());
        abort_line(&mut t, 10, 0x37c_aef0, true).unwrap();
        abort_line(&mut t, 11, 0x12_3456, false).unwrap();
        assert_eq!(
            t.0,
            "10 abort caller=main+0x37caef0\n11 abort caller=0x123456\n"
        );
    }

    #[test]
    fn verdict_codes_round_trip() {
        for verdict in [
            Verdict::Keep,
            Verdict::Repair(0x1234),
            Verdict::Empty,
            Verdict::Unfixable,
        ] {
            let (code, to) = verdict_code(verdict);
            assert_eq!(verdict_from_code(code, to), verdict);
        }
    }

    #[test]
    fn free_lines_text() {
        let mut failure = FreeFailure {
            result: 0xd401,
            address: 0x11_8000_0000,
            size: 0x5_0000,
            total: 0xcd50_0000,
            used: 0xc000_0000,
            block_count: 1,
            ..FreeFailure::default()
        };
        failure.blocks[0] = Block {
            address: 0x11_8000_0000,
            size: 0x10_0000,
            state: 0x5,
            attributes: 0xc,
            permission: 3,
            ipc_references: 0,
            device_references: 1,
        };
        let mut t = Text(String::new());
        free_lines(&mut t, 9, &failure).unwrap();
        assert_eq!(
            t.0,
            "9 free_pages_refused result=0xd401 (2001-0106) address=0x1180000000 size=0x50000 total=0xcd500000 used=0xc0000000\n\
             9 free_pages_block i=0 address=0x1180000000 size=0x100000 state=0x5 attributes=0xc permission=0x3 ipc_references=0 device_references=1\n"
        );
    }
}
