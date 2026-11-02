# ssbu-memory-guard

> **Disclaimer:** this project was developed with the assistance of AI
> (Claude, Codex and other coding agents) under the operator's direction and
> review. Nothing is claimed beyond the dated tests cited below.

A Skyline plugin for Super Smash Bros. Ultimate 13.0.5 (`libmemory_guard.nro`).
At seven places where the game crashed on the operator's Switch because a
pointer had one stray bit, it checks the pointer just before the game reads
it and puts it back: a real address with one extra high bit loses that bit,
and an empty pointer that gained one bit becomes empty again, so the game
takes its normal path. Anything it cannot put right is logged and left alone.
It also records why the game's heap failed to give memory back when that
failure stops the game at startup.

![Training keeps running after seven planted faults were repaired](docs/media/q03-training.gif)

*Eden, 2026-11-02, run q03: the card's 16 plugins plus this plugin's
diagnostic build (`9aeca255`). A stray bit was planted once at each of the
seven sites; each was repaired, and Training ran on with the CPU Pilot
driving P1.*

- **Only values the game would crash on.** A value is changed only when it
  is above the console's 39-bit address space with one stray bit over a
  mapped address, or one lone bit where nothing is mapped. A pointer the game
  can read is never touched ([the rule](src/model.rs)).
- **Seven sites,** each from a crash report on the card: the render
  dispatcher's command-buffer list, a component getter, a list walk, a field
  getter and a hash-table rehash.
- **Startup aborts:** when the kernel refuses a page release, the plugin
  writes the result, the range and the kernel's view of those pages before the
  game's own `abort()` runs. Any other `abort()` caller is logged by address.
- **Guarded:** 13.0.5 only; every hooked instruction and every instruction a
  hook relies on is compared with the 13.0.5 bytes first, and another
  plugin's hook there is chained onto. Hooks go only on plain instructions
  and only as one-word branches.
- **Cost:** 64 KiB of game memory, no heap use. Each hook runs through
  Skyline's trampoline; the busiest site was called up to about 42,000 times
  a second in Eden ([numbers](docs/EVIDENCE.md)).

**Tested** in Eden on 2026-11-02 ([evidence](docs/EVIDENCE.md)): every site
installed next to the card's plugins; planted stray bits were repaired at all
seven sites; with repairs off the same plants made Eden read the addresses the
console crashed on; a forced page-release refusal was recorded; a LAN Online
match ended with no desync. Installed on the operator's Switch on 2026-11-02;
**not yet run** there.

## Log

`sd:/ultimate/memory-guard/events.log`:

```text
<tick> plugin=memory-guard/0.1.0 dispatch_list=stock/near … abort_stub=stock/near result=installed heap_allocations=0
<tick> guard site=dispatch_list verdict=repair value=0x46ad817c790 to=0x6ad817c790 slot=0x6b016d2408
<tick> guard site=list_next verdict=empty value=0x20000000000 to=0x0 slot=…
<tick> guard site=… verdict=unfixable value=…            (the game will fault on it)
<tick> free_pages_refused result=0xd401 (2001-0106) address=… size=… total=… used=…
<tick> free_pages_block i=0 address=… size=… state=… attributes=… permission=… ipc_references=… device_references=…
<tick> abort caller=main+0x…
```

## Install

[Download 0.1.0](https://git.syntaxbyexample.com/intpa/ssbu-memory-guard/releases/tag/v0.1.0)
and copy `libmemory_guard.nro` to
`sd:/atmosphere/contents/01006A800016E000/romfs/skyline/plugins/`.

## Build

```text
cargo test
cargo skyline build --release
cargo skyline build --release --features diag   # Eden: hit counts, planted faults (diag.txt)
```

`src/guards.rs` is generated: `python3 -I tools/guard_table.py <13.0.5 main> > src/guards.rs`.

## License

MIT
