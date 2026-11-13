# How it works

2026-11-13, for 0.1.0 (`main` at `fd1b9a6`, the same source as tag `v0.1.0`),
plus the 0.2.0 commit-side record (released as `v0.2.0` on 2026-11-13 and
installed on the card; untested on hardware, not yet played).
Moved here from the README; each point names the code it describes.

## The rule

`model::classify` (`src/model.rs`) changes a value only when the game would
fault on it:

- above the console's 39-bit address space with one stray high bit over an
  address where 0x18 bytes can be read (`svcQueryMemory`): that address, the
  bit cleared (`repair`);
- one stray high bit and nothing under it, or one lone bit inside the address
  space (at least `0x10000`) where nothing is mapped: empty (`empty`), so the
  game takes its path for an empty pointer;
- anything else above the address space: logged and left (`unfixable`). At
  `dispatch_list` an entry cannot be emptied, so `empty` counts as
  `unfixable` there.

A value the game can read through is never changed.

## The sites

Seven pointer sites, each the instruction that loads the pointer (the slot is
checked and repaired before the load), plus `abort_stub` (`model::SITES`):
`dispatch_list` `0x3816814`, `component_get` `0x3810c`, `list_head`
`0x37783b0`, `list_next` `0x37783bc`, `field_a8` `0x59860`, `rehash_head`
`0x391756c`, `rehash_next` `0x391758c`, `abort_stub` `0x39c2654`. The
diagnostic build adds `free_pages_force` `0x37cae1c`.

- **Guards:** nothing is installed unless the game reports 13.0.5. Each site
  and each window of instructions a hook relies on is compared, 64 bytes at
  a time, with `src/guards.rs`; a site that differs is left alone
  (`<site>=foreign/skipped` or `relied/skipped`). On 2026-11-13 all 14
  windows in `src/guards.rs` matched the 13.0.5 `.text`.
- **One-word hooks on plain instructions:** no site is a PC-relative
  instruction, and a hook Skyline writes as a far jump is taken out again
  (`chain::hook::inline`, `near_only`). Eden q01 showed why (docs/EVIDENCE.md).
- **Another plugin's hook on a site** is chained onto (`src/chain.rs`:
  gsp-visualizer 0.3.0's file with one stub per site and `near_only`). No
  Eden run has exercised this path for this plugin.
- **Refused page release:** `abort_stub` sits in `abort`'s import stub. When
  `abort` returns to `0x37caef0` (after `nn::os::FreeMemoryPages` failed) the
  hook records the result, the range, the process's memory totals and up to
  six blocks the kernel reports, and waits up to 1.5 s for the writer before
  the abort goes on. Any other caller is logged by return address.
- **Refused page commit (0.2.0):** the same provider calls `abort` (return
  address `0x37ca68c`) when `nn::os::AllocateMemoryPages` fails; it branches
  there straight from `cbnz w0`, so `w0` is still the result, `x21` the
  address and `x19` the size in bytes. The hook records those and
  `svcGetInfo` 6/7 (process memory) and 16/17 (the system resource: page
  tables and memory blocks). The game holds the provider's mutex there, so the
  hook takes no lock and allocates nothing, and waits at most 1.5 s for the
  writer. This needs the commit's instructions to be 13.0.5's
  (`model::ALLOC_PAGES_RELIED`, read at install: `alloc_pages=known`); when
  they differ, the abort is logged by return address only
  (`alloc_pages=relied/skipped`). Eden 2026-11-13 (`mg020-diag-1`): the
  diag build's `force_commit_failure` refused one commit on purpose, and the
  line came out with the real address and size (docs/EVIDENCE.md). Not seen
  on the Switch yet.
- **Logging:** hooks hand a few words to a writer thread through a fixed
  ring of 32 slots; only that thread writes the file.

## Log

`sd:/ultimate/memory-guard/events.log` (`model::event_line`,
`model::free_lines`, `model::commit_line`, `model::abort_line`,
`runtime::main`). From 0.2.0 the install line also carries
`alloc_pages=known|relied/skipped` before `result=`. The
`dispatch_list` line below is built from the 2026-11-02 15:02 console crash
(its value, and entry 65 of the list as the slot); the plugin had not logged
a repair on the console as of 2026-11-03.

```text
<tick> plugin=memory-guard/0.1.0 dispatch_list=stock/near … abort_stub=stock/near result=installed heap_allocations=0
<tick> guard site=dispatch_list verdict=repair value=0x46ad817c790 to=0x6ad817c790 slot=0x6b016d2408
<tick> guard site=list_next verdict=empty value=0x20000000000 to=0x0 slot=…
<tick> guard site=… verdict=unfixable value=…            (the game will fault on it)
<tick> free_pages_refused result=0x… (<2000+module>-<description>) address=… size=… total=… used=…
<tick> free_pages_block i=0 address=… size=… state=… attributes=… permission=… ipc_references=… device_references=…
<tick> alloc_pages_refused caller=main+0x37ca68c result=0x… (<2000+module>-<description>) address=… size=… total=… used=… slab_total=… slab_used=…
<tick> abort caller=main+0x…
```

`result=` in the install line is `installed`, `partial` or `not_installed`
(`unsupported_version` on another game version). A `guard` line is written
when the hook decides; a concurrent change of the slot can still keep the
repair from being stored (`runtime::swap_slot` ignores the compare-exchange
result).
