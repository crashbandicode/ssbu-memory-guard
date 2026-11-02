# Evidence

## Why these sites (2026-11-02)

The operator's card held 186 Atmosphère crash reports for Smash; 94 were
13.0.5. Read over DBI and classified on the host (scripts and copies in the
work folder, not in this repo):

- One report faulted on a real pointer with one stray bit (2026-11-02 15:02,
  render dispatch, `main+0x381686c`: entry 65 of the frame's command-buffer
  list was `0x46ad817c790`, an address with bit 42 set).
- Four faulted on an empty pointer that gained one bit, right after the
  game's own "is it empty?" check passed because of it (2026-10-06 bit 42 and
  bit 25, 2026-10-15 bit 41, 2026-10-23 bit 28). The code at each site was
  read in the 13.0.5 image to confirm the check.
- Twelve were the game's `abort()` after `nn::os::FreeMemoryPages` failed in
  the heap page provider (`main+0x37cae20`, all at startup, the latest
  2026-11-02 15:42).
- None of the other 77 has a value this plugin could put right (plugin bugs,
  the game stopping itself, missing objects, memory that was never there).

**Approved workaround (2026-11-02):** the operator asked for this plugin.
The root cause of the stray bits is not known (single bits at positions 25,
28, 41 and 42, always 0 → 1); the plugin repairs the value where the game
would fault on it and logs each repair, which is also how the cause can be
narrowed down. The refused release is only recorded, not changed.

Card scan before choosing the sites (`tools/build/scan-offsets.py` from the
playbook, the card's 16 NROs as read 2026-11-02, hashes checked): no plugin
refers to any hooked offset. The Training Modpack's site table names
`0x37cae24` (its `console_map` probe) but its install line marks it
`unchecked`, not installed.

## Eden, 2026-11-02 (the Ally, the card's 16 plugins plus the tile cursor)

Each run boots the card's set with this plugin, goes to Training, runs the CPU
Pilot hand-over and 40 s with the CPU driving P1 (harness from the
ssbu-cpu-pilot h02 run).

| Run | Build | What | Result |
| --- | --- | --- | --- |
| q01 | diag `061d037c` (`f286b52`) | first sites, hooked on branch instructions | **Failed:** Smash never drew a frame. A taken `cbnz` that Skyline's inline hook had moved into its trampoline jumped 0x200000 past its target (`x17 = main+0x3b17578`, in rodata). Fixed in `be9af75`: only plain instructions are hooked, and only as one-word branches. |
| q02 | diag `fb2278bc` (`be9af75`) | no plants | `result=installed heap_allocations=0`, every site `stock/near`; all steps done; no guard line; `eden_log.txt` without `Unmapped`. |
| q03 | diag `9aeca255` (`2eebbfb`) | stray bits planted once at all seven game sites | Seven `diag inject` lines, seven repairs: `dispatch_list` and `component_get` back to the exact pointer (`0x411d8162da0 → 0x11d8162da0`, `0x411ea85e6e0 → 0x11ea85e6e0`), the five others back to empty (bits 41, 25, 28). All steps done; no `Unmapped`. |
| q04 | diag `9aeca255` | the same plants, `repair=off` | The rehash plant (bit 28) went through: Eden read `0x1000000c` (the fault address of the 2026-10-23 console crash), the bad pointer spread into other buckets, and the game never reached the character select. |
| q05 | diag `9aeca255` | the 100th page release refused (address 0 passed) | `free_pages_refused result=0xe03 (2003-0007) address=0x11fe110000 size=0x30000 total=0xcd500000 used=0x8c3eb000 forced=1` and the block line, written before the game's own abort. |
| q06 | diag `9aeca255` | `dispatch_list` plant (bit 42), `repair=off` | Eden read `0x411d8142da8` (the planted pointer + 8, the shape of the 2026-11-02 console crash), then jumped to address 0: the game stopped. |
| q07 | **release `b2de129a`** (`2eebbfb`) | the shipped build, no plants | `result=installed heap_allocations=0`, all eight sites `stock/near`; all steps done; the log held only the install line; no `Unmapped`. |

Calls per second, `python3 -I tools/hits.py <events.log>` over q02 and q03
(5 s samples, Training with the CPU fighting):

| Site | Mean | Peak |
| --- | --- | --- |
| `field_a8` | 10,020–10,971 | 42,375–42,391 |
| `dispatch_list` | 965–1,074 | 1,502 |
| `list_head` | 521–553 | 13,463–14,423 |
| `list_next` | 438–465 | 11,367–12,226 |
| `component_get` | 195–208 | 3,918–3,927 |
| `rehash_head`, `rehash_next` | 23–114 | 383–3,201 |

Each call goes through Skyline's inline-hook trampoline (all registers saved
and restored) and a few compares; the cost per call on the console is not
measured.

## Eden LAN pair, 2026-11-02 (d02)

One LAN Online Quickplay match (1 stock) between the Surface and the Yoga. The
Surface ran release `b2de129a` with Online Deluxe 0.10.2 (`bc8a6428…`),
ssbu-desync-detector `95e7def9…` and the tile cursor; the Yoga ran Online
Deluxe 0.10.1 (`a8dcda24…`), the same detector and cursor, without this
plugin.

- The Surface's log held only the install line: all eight sites
  `stock/near`, `result=installed heap_allocations=0`.
- Both detectors: `sync_end trigger=normal kind=1 frame=1218 …
  desync_seen=0`.
- Neither Eden log had an `Unmapped` line.

## Release and card install, 2026-11-02

- Tag `v0.1.0` (`f0aee45`) built from a clean clone byte-identical to the
  tested release build (`b2de129a…`, 40,960 bytes).
- The [Forgejo release](https://git.syntaxbyexample.com/intpa/ssbu-memory-guard/releases/tag/v0.1.0)
  (release 649) carries the NRO, `manifest.json` (`c2f628a7…`,
  `hardware_tested: false`) and `SHA256SUMS.txt`; each was downloaded without
  authentication and matched.
- Added to the operator's card over hekate UMS on the Surface (17:20Z,
  `installed_verified`, then `verified_in_place`), next to the card's 16
  plugins (Online Deluxe 0.10.2 among them), which read back unchanged.
  To remove it, delete `libmemory_guard.nro` from the plugins folder.

**Not tested:** a run on the Switch (installed, not yet booted); online play
beyond the LAN pair; a stray bit the game produced by itself (every repair
above was planted).
