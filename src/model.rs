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
//! In every case the game had just checked the pointer for empty, or was
//! about to read through it, and faulted. The rule here changes only such a
//! value, and only to what it was before the bit appeared:
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
//! `free_pages_abort` is the other kind of startup crash: the game's heap page
//! provider (`main+0x37c9600`) gives pages back with `nn::os::FreeMemoryPages`
//! and calls `abort()` when the kernel refuses (12 reports, 2026-10-13 to
//! 2026-11-02). The hook there only records the refusal and the state of the
//! pages, then lets the abort go on.

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
    /// `x26..x27` is the frame's list of command-buffer pointers (each read at
    /// `+0x8` next); entries are checked and repaired in place. An entry
    /// cannot be emptied, so `Empty` counts as `Unfixable` here.
    PointerList { begin: u8, end: u8 },
    /// Before `ldr x0, [base, offset]`: the slot is checked, and repaired in
    /// place, before the game loads it.
    LoadSlot { base: u8, offset: u8 },
    /// After a pointer load: `reg` holds the value; `slot` (`base` register,
    /// byte offset) still holds it in memory; `copy` is a second register the
    /// game already copied it to.
    Loaded {
        reg: u8,
        slot: Option<(u8, u16)>,
        copy: Option<u8>,
    },
    /// `bl abort` after `FreeMemoryPages` failed: `w0` the result, `x20` the
    /// address, `x21` the size. Recorded, then the abort goes on.
    FreePagesAbort,
    /// Diag only: the free result check (`cbnz w0`), to force the failure path
    /// in Eden.
    FreePagesResult,
}

/// A hooked site and the 64 bytes there in 13.0.5, plus a 64-byte window
/// holding the instructions the hook relies on (the loads that give a register
/// its meaning), which must be the 13.0.5 ones up to the site.
pub struct Site {
    pub name: &'static str,
    pub offset: usize,
    pub guard: [u8; 64],
    pub relied_offset: usize,
    pub relied: [u8; 64],
    /// Instructions at the start of the relied-on window that must be exactly
    /// the 13.0.5 ones; from there on, other plugins' hooks are accepted.
    pub relied_kept: usize,
    pub action: Action,
    /// The words after the site are a branch target: only a one-word hook is
    /// safe there (a far jump would cover the target).
    pub near_only: bool,
    /// Installed only in the diagnostic build.
    pub diag_only: bool,
}

impl Site {
    /// The 13.0.5 instructions at the site.
    pub fn stock(&self) -> [u32; chain::WORDS] {
        chain::words(&self.guard)
    }

    /// The 13.0.5 instructions of the relied-on window.
    pub fn relied_stock(&self) -> [u32; chain::WORDS] {
        chain::words(&self.relied)
    }

    /// Whether the relied-on window holds the site itself.
    pub fn relied_holds_site(&self) -> bool {
        (self.relied_offset..self.relied_offset + 64).contains(&self.offset)
    }
}

/// Every site, in install order. Indices are the hook and stub numbers.
pub const SITES: [Site; 9] = [
    Site {
        name: "dispatch_list",
        offset: 0x381_6814,
        guard: guards::DISPATCH_LIST,
        relied_offset: 0x381_67e8,
        relied: guards::DISPATCH_LIST_RELIED,
        relied_kept: 11,
        action: Action::PointerList { begin: 26, end: 27 },
        near_only: false,
        diag_only: false,
    },
    Site {
        name: "component_get",
        offset: 0x3_810c,
        guard: guards::COMPONENT_GET,
        relied_offset: 0x3_8100,
        relied: guards::COMPONENT_GET_RELIED,
        relied_kept: 3,
        action: Action::LoadSlot { base: 0, offset: 8 },
        near_only: false,
        diag_only: false,
    },
    Site {
        name: "list_head",
        offset: 0x377_83b4,
        guard: guards::LIST_HEAD,
        relied_offset: 0x377_83b0,
        relied: guards::LIST_RELIED,
        relied_kept: 1,
        action: Action::Loaded {
            reg: 8,
            slot: Some((21, 8)),
            copy: None,
        },
        near_only: true,
        diag_only: false,
    },
    Site {
        name: "list_next",
        offset: 0x377_83c0,
        guard: guards::LIST_NEXT,
        relied_offset: 0x377_83b0,
        relied: guards::LIST_RELIED,
        relied_kept: 4,
        action: Action::Loaded {
            reg: 8,
            slot: Some((21, 0)),
            copy: None,
        },
        near_only: true,
        diag_only: false,
    },
    Site {
        name: "field_a8",
        offset: 0x5_9864,
        guard: guards::FIELD_A8,
        relied_offset: 0x5_9860,
        relied: guards::FIELD_A8_RELIED,
        relied_kept: 1,
        action: Action::Loaded {
            reg: 8,
            slot: Some((0, 0xa8)),
            copy: None,
        },
        near_only: false,
        diag_only: false,
    },
    Site {
        name: "rehash_head",
        offset: 0x391_7574,
        guard: guards::REHASH_HEAD,
        relied_offset: 0x391_756c,
        relied: guards::REHASH_RELIED,
        relied_kept: 2,
        action: Action::Loaded {
            reg: 9,
            slot: None,
            copy: None,
        },
        near_only: true,
        diag_only: false,
    },
    Site {
        name: "rehash_next",
        offset: 0x391_75a0,
        guard: guards::REHASH_NEXT,
        relied_offset: 0x391_756c,
        relied: guards::REHASH_RELIED,
        relied_kept: 13,
        action: Action::Loaded {
            reg: 13,
            slot: None,
            copy: Some(9),
        },
        near_only: true,
        diag_only: false,
    },
    Site {
        name: "free_pages_abort",
        offset: 0x37c_aeec,
        guard: guards::FREE_PAGES_ABORT,
        relied_offset: 0x37c_ae10,
        relied: guards::FREE_PAGES_RELIED,
        relied_kept: 5,
        action: Action::FreePagesAbort,
        near_only: false,
        diag_only: false,
    },
    Site {
        name: "free_pages_result",
        offset: 0x37c_ae24,
        guard: guards::FREE_PAGES_RESULT,
        relied_offset: 0x37c_ae10,
        relied: guards::FREE_PAGES_RELIED,
        relied_kept: 5,
        action: Action::FreePagesResult,
        near_only: true,
        diag_only: true,
    },
];

/// The site table index of a name (diag settings name sites).
pub fn site_index(name: &[u8]) -> Option<usize> {
    SITES.iter().position(|site| site.name.as_bytes() == name)
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
    /// Set by the diagnostic build's forced failure (Eden); the console never sets it.
    pub forced: bool,
}

/// Diagnostic settings (`diag.txt`, Eden only).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DiagSettings {
    /// Plant a stray bit once at this site.
    pub inject: Option<usize>,
    /// Plant it on an empty pointer (`true`) or on a real one (`false`).
    pub inject_empty: bool,
    /// Which bit to plant.
    pub inject_bit: u32,
    /// `repair=off`: let the planted value through (the control run).
    pub repair_off: bool,
    /// Make the page release at `free_pages_result` fail once, after this many releases.
    pub force_free_failure: Option<u64>,
}

impl DiagSettings {
    pub fn parse(text: &[u8]) -> Self {
        let mut out = Self {
            inject_bit: 42,
            ..Self::default()
        };
        for line in text.split(|b| *b == b'\n') {
            let Some(eq) = line.iter().position(|b| *b == b'=') else {
                continue;
            };
            let (key, value) = (line[..eq].trim_ascii(), line[eq + 1..].trim_ascii());
            match key {
                b"inject" => out.inject = site_index(value),
                b"inject_mode" => out.inject_empty = value == b"empty",
                b"inject_bit" => {
                    if let Some(bit) = parse_u64(value).filter(|b| (16..64).contains(b)) {
                        out.inject_bit = bit as u32;
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

    fn word(guard: &[u8; 64], index: usize) -> u32 {
        chain::words(guard)[index]
    }

    /// `cbz`/`cbnz` (64-bit) on register `rt`.
    fn is_cb(word: u32, nonzero: bool, rt: u32) -> bool {
        word & 0xff00_001f == if nonzero { 0xb500_0000 } else { 0xb400_0000 } | rt
    }

    #[test]
    fn each_site_holds_the_instruction_its_action_expects() {
        let by_name = |n: &str| SITES.iter().find(|s| s.name == n).unwrap();
        assert_eq!(
            word(&by_name("dispatch_list").guard, 0),
            0xeb1b_035f,
            "cmp x26, x27"
        );
        assert_eq!(
            word(&by_name("dispatch_list").relied, 0),
            0xa941_6ffa,
            "ldp x26, x27, [sp, #0x10]"
        );
        assert_eq!(
            word(&by_name("component_get").guard, 0),
            0xf868_6800,
            "ldr x0, [x0, x8]"
        );
        assert!(is_cb(word(&by_name("list_head").guard, 0), false, 8));
        assert_eq!(
            word(&by_name("list_head").relied, 0),
            0xf940_06a8,
            "ldr x8, [x21, #8]"
        );
        assert!(is_cb(word(&by_name("list_next").guard, 0), true, 8));
        assert_eq!(
            word(&by_name("list_next").relied, 2),
            0xaa08_03f5,
            "mov x21, x8"
        );
        assert_eq!(
            word(&by_name("list_next").relied, 3),
            0xf940_0108,
            "ldr x8, [x8]"
        );
        assert!(is_cb(word(&by_name("field_a8").guard, 0), false, 8));
        assert_eq!(
            word(&by_name("field_a8").relied, 0),
            0xf940_5408,
            "ldr x8, [x0, #0xa8]"
        );
        assert!(is_cb(word(&by_name("rehash_head").guard, 0), false, 9));
        assert_eq!(
            word(&by_name("rehash_head").relied, 0),
            0xf86c_6969,
            "ldr x9, [x11, x12]"
        );
        assert_eq!(
            word(&by_name("rehash_head").relied, 1),
            0xf82c_697f,
            "str xzr, [x11, x12]"
        );
        assert!(is_cb(word(&by_name("rehash_next").guard, 0), true, 13));
        assert_eq!(
            word(&by_name("rehash_next").relied, 8),
            0xf940_092d,
            "ldr x13, [x9, #0x10]"
        );
        assert_eq!(
            word(&by_name("rehash_next").relied, 12),
            0xaa0d_03e9,
            "mov x9, x13"
        );
        assert_eq!(
            word(&by_name("free_pages_abort").guard, 0) >> 26,
            0b100101,
            "bl abort"
        );
        assert_eq!(
            word(&by_name("free_pages_result").guard, 0) & 0xff00_001f,
            0x3500_0000,
            "cbnz w0"
        );
        assert_eq!(
            word(&by_name("free_pages_abort").relied, 1),
            0xd370_bd15,
            "lsl x21, x8, #16"
        );
    }

    #[test]
    fn relied_windows_end_where_the_site_starts() {
        for site in &SITES {
            assert!(site.relied_offset <= site.offset, "{}", site.name);
            assert!(site.relied_kept <= chain::WORDS, "{}", site.name);
            assert_eq!(site.offset % 4, 0);
            if site.relied_holds_site() {
                // Exactly the words before the site are kept; the site's own
                // word is in both windows.
                assert_eq!(
                    site.relied_kept,
                    (site.offset - site.relied_offset) / 4,
                    "{}",
                    site.name
                );
                assert_eq!(
                    site.relied_stock()[site.relied_kept],
                    site.stock()[0],
                    "{}",
                    site.name
                );
            }
        }
        // The abort site relies on the release call 0xdc bytes before it:
        // mov w8, w21; lsl x21, x8, #16; mov x0, x20; mov x1, x21; bl FreeMemoryPages.
        let abort = &SITES[site_index(b"free_pages_abort").unwrap()];
        assert!(!abort.relied_holds_site());
        assert_eq!(abort.relied_kept, 5);
        assert_eq!(abort.relied_stock()[4] >> 26, 0b100101, "bl");
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
            b"inject=list_next\ninject_mode=empty\ninject_bit=25\nrepair=off\n",
        );
        assert_eq!(s.inject, site_index(b"list_next"));
        assert!(s.inject_empty && s.repair_off);
        assert_eq!(s.inject_bit, 25);
        let s = DiagSettings::parse(b" force_free_failure = 40 \r\ninject_bit=3\n");
        assert_eq!(s.force_free_failure, Some(40));
        assert_eq!(s.inject_bit, 42, "bits below 16 are refused");
        assert_eq!(DiagSettings::parse(b"inject=nope").inject, None);
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
