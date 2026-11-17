# Concurrency and real-time work: Rust practice vs protoHDMIcapture

Research note, 2026-11-17: modern Rust practice (2025-2026) against
protoHDMIcapture, one of three ([concurrency](rust-concurrency.md),
[graphics](rust-graphics.md), [capture and audio](rust-capture-audio.md)).
Findings, not decisions: each verdict is a proposal for the grilling and
the issues that follow. Line numbers are from `14a5301`. Nothing here was
measured on a card; every "measure first" still needs its run
([`docs/testing.md`](../testing.md)).

Sources were fetched 2026-11-17.
Items marked **UNVERIFIED** were not read from a primary page this session.

Source notes: the Ross Bencina origin page (rossbencina.com) refused connections
from this environment, so its rules are quoted from a reader mirror. The Rust 1.67
blog page (blog.rust-lang.org) returned 404 on direct fetch; its statement is
taken from search snippets plus releases.rs. The Mara Bos book was read through
its redirect host (mara.nl), not marabos.nl directly.

Crate versions (docs.rs, latest at fetch time): cpal 0.18.2 (Cargo.lock pins
0.18.2), ctrlc 3.5.2, rtrb 0.4.1, ringbuf 0.6.0, crossbeam-channel 0.5.17,
triple_buffer 9.0.1, arc-swap 1.9.5, signal-hook 0.4.6.

---

## 1. rust low-latency video pipeline threads channels

**Best practice.** A live viewer wants the newest frame, not a queue. A
bounded or unbounded channel of frames adds latency and memory growth when the
consumer is slow. The usual pattern is a single "latest value" slot, plus a
coalesced wake so the consumer is poked at most once per pending frame.
- std `sync_channel` is a pre-allocated bounded buffer with blocking sends
  (https://doc.rust-lang.org/std/sync/mpsc/index.html). Unbounded `channel` "conceptually has an infinite buffer" (same page).
- triple_buffer describes a "non-blocking thread synchronization mechanism"
  where the consumer reads "the latest available version" of a block updated by
  one producer (https://docs.rs/triple_buffer/latest/triple_buffer/). It does not
  claim wait-free in the text fetched (UNVERIFIED on that property).

**What the codebase does.** Capture thread writes the frame into one
`Arc<Mutex<Slot>>` (`src/run.rs:110-114`, `:174`), then wakes the window
(`src/run.rs:188-195`). The window takes the frame by `mem::swap` under the
same lock (`src/app.rs:1272-1275`). Wakes are coalesced: an `AtomicBool`
`frame_wake` lets at most one Frame wake wait (`src/app.rs:419-421`,
`:1470` clears it), then they go through winit's `EventLoopProxy`
(`src/app.rs:457-465`). A `mpsc` channel is used only for the placer
(`src/app.rs:1114-1153`), not for frames.

**Verdict: keep (the slot design); consider one change.**
- Keep: latest-frame-only is the right semantics for a viewer, and the
  single slot avoids queue latency.
- Consider: the capture thread copies the whole frame *under the lock*
  (`src/run.rs:188-192`, `s.data.copy_from_slice(f.data)`). The window's
  critical section is only the swap, but it can wait for an in-progress copy.
  At 1920x1080 YUYV one frame is 4,147,200 bytes.
  - Change: capture writes into a spare buffer outside the lock, then swaps
    the buffer pointer under the lock (O(1)). Or use `triple_buffer`.
  - Trade-off: one extra frame buffer and a return path for the spare. Measure
    first with the dashboard (`src/dashboard.rs`) before changing; there is no
    evidence in the repo that this copy causes a visible stall.
- Lost-wake check (reasoned from the code, not tested beyond
  `app.rs:1776 one_frame_wake_waits_at_a_time`): the ordering of the
  `pending` swap and the `Release` store at `app.rs:1470` with the slot lock
  leaves no interleaving that loses a frame. Keep as is.

---

## 2. rust lock-free ring buffer audio thread

**Best practice.** Between two real-time callbacks, use a wait-free or
lock-free SPSC ring. A mutex in a callback is the thing to avoid.
- rtrb: "A realtime-safe single-producer single-consumer ring buffer";
  "Reading from and writing into the ring buffer is _lock-free_ and _wait-free_";
  "A fixed-capacity buffer is allocated on construction"; "Only a single thread
  can write"; there is no way to block waiting for data
  (https://docs.rs/rtrb/latest/rtrb/). It is SPSC only, so the overflow policy
  must live on the consumer side.
- ringbuf: "Lock-free SPSC FIFO ring buffer with direct access to inner data"
  (https://docs.rs/ringbuf/latest/ringbuf/). Random read by index was not
  confirmed on the page.
- Bencina's rule, via mirror: "use lock-free FIFO queues to communicate commands
  and data between real-time and non-real-time contexts"
  (https://hn.jakechampion.name/reader/http://www.rossbencina.com/code/real-time-audio-programming-101-time-waits-for-nothing).
- Ordering primer: Relaxed is "No ordering constraints, only atomic operations";
  Release/Acquire pairing makes later operations ordered after the store
  (https://doc.rust-lang.org/std/sync/atomic/enum.Ordering.html). Mara Bos, Rust
  Atomics and Locks, on the same point: a Relaxed flag does not order other data
  (http://mara.nl/atomics/memory-ordering.html).

**What the codebase does.** One `Arc<Mutex<Ring>>`, where `Ring` holds a
`VecDeque<[f32;2]>` (`src/audio.rs:596`). The input callback locks it, extends
it, then drains the excess (`src/audio.rs:605-614`). The output callback locks
it and runs `play_out` (`src/audio.rs:626`, `:473-512`). `play_out` does
fractional-position resampling with `r.frames.get(k)` and `drain`
(`src/audio.rs:494-511`), which needs indexed reads. Both callbacks run on
cpal's real-time threads; `Cargo.toml` enables `cpal` `realtime-dbus` for Linux.

**Verdict: change (consider now, gated on a hardware run).**
- The mutex is held on an RT thread on both sides. Bencina says not to use
  mutexes in a callback because of priority inversion
  (https://hn.jakechampion.name/reader/http://www.rossbencina.com/code/real-time-audio-programming-101-time-waits-for-nothing).
  Both sides are RT threads, so the inversion risk is bounded, not absent,
  when one side is not promoted (`RealtimeDenied`, see section 4).
- Concrete change: swap the mutex for an rtrb `Producer<[f32;2]>` in the input
  callback and `Consumer` in the output callback. Keep the fractional `pos` and
  the step logic as state owned by the output closure. Two consequences:
  1. The input callback can no longer drop the excess from the front. Move
     overflow discard to the output callback: when fill exceeds the max, pop and
     discard on that side.
  2. Random access to `frames.get(k)` is not available on a plain rtrb
     consumer. Copy the needed window into a preallocated scratch array
     (sized from `buffer_frames`) and interpolate from that.
- Trade-off: a moderate rewrite of `play_out` and the input closure. It has to
  be re-proven on the card (CLAUDE.md, docs/testing.md), and it changes the
  drift-steering behaviour that the code comments tune (`DEAD_BAND`, `GAIN`,
  `CATCH_UP`). The unit tests in `audio.rs` cover counters, not the ring's
  steering, so they would need new ones.

---

## 3. rust Arc<Mutex> vs atomics vs crossbeam channel

**Best practice.**
- Atomics for independent counters and flags that carry no other data
  (Relaxed is enough): ordering docs quoted above; Mara Bos on Relaxed.
- Mutex for a value that must change together (a small struct, an Option);
  `lock()` blocks and returns `Err` on poisoning
  (https://doc.rust-lang.org/std/sync/struct.Mutex.html).
- std `mpsc` has been crossbeam-based since Rust 1.67, with no public API
  change (https://releases.rs/docs/1.67.0; the blog at
  https://blog.rust-lang.org/2023/01/26/Rust-1.67.0/ was taken from search
  snippets, not fetched directly). So crossbeam-channel is not automatically
  faster for the common case. crossbeam-channel describes itself as "an
  alternative to std::sync::mpsc with more features and better performance"
  (https://docs.rs/crossbeam-channel/latest/crossbeam_channel/). The benefit
  is `select!` across several channels.
- arc-swap: "optimized for read-mostly scenarios"; it "has better performance
  characteristics than the RwLock" in the page's own words, with no read-side
  figures (https://docs.rs/arc-swap/latest/arc_swap/).

**What the codebase does.**
- Atomics for counters and status: `Counters` (`src/audio.rs:79-93`),
  dashboard `Live` (`src/dashboard.rs:91-101`), the capture policy's counters
  (`src/capture/policy.rs:107-108`), `stop` and `closing` flags (`src/run.rs:176`,
  `src/audio.rs:93`). Relaxed throughout.
- Mutex for shared Option/Vec/String slots: `LevelSlot` and `NotifySlot`
  (`src/audio.rs:698`, `:110`), notices (`src/app.rs:260`), dashboard `samples`,
  `events`, `audio` (`src/dashboard.rs:103-105`).
- std `mpsc` for the placer (`src/app.rs:1115`) and the level worker
  (`src/audio.rs:721`). No crossbeam dependency.

**Verdict: keep.** The split is correct: atomics for counters, a mutex for
multi-field values, and no RT path uses a mutex except the ring (section 2).
No reason found to add crossbeam. Note `expect("... never poisoned")` is used
throughout; a panic inside a locked section would poison it and panic the
next locker. That is acceptable for a viewer, but say so in the code if the
section can panic (the copy in `src/run.rs:192` can, on a length mismatch).

---

## 4. rust real-time audio thread no allocation

**Best practice.**
- Bencina, via mirror: "Don't allocate or deallocate memory"; "you should not
  allocate memory in your audio callback"; no mutex; no blocking OS calls; no
  I/O; "this includes things like calling printf or NSLog" (logging)
  (https://hn.jakechampion.name/reader/http://www.rossbencina.com/code/real-time-audio-programming-101-time-waits-for-nothing).
- cpal 0.18.2: the `realtime` feature "raises the audio callback thread to
  real-time or high-priority scheduling"; `realtime-dbus` uses rtkit on Linux;
  ALSA's `RealtimeDenied` is emitted "only when promotion is attempted but
  fails" (https://docs.rs/crate/cpal/latest). The docs say nothing about
  allocation in callbacks (UNVERIFIED beyond this).
- Secondary write-up (UNVERIFIED, single author): replacing an mpsc in a cpal
  callback with a preallocated `ringbuf::HeapRb` removed glitches
  (https://tindotdev.pages.dev/writing/zero-allocation-audio-callbacks-in-rust).

**What the codebase does.**
- `play_out` (`src/audio.rs:473-512`) does not allocate: indexing, `drain`,
  atomics. It does take the ring mutex.
- The input callback `r.frames.extend(...)` (`src/audio.rs:609`) can
  reallocate. The ring is created with `VecDeque::with_capacity(ring_frames(...))`
  (`src/audio.rs:596`). After each input callback the length is trimmed to
  `ring_frames` (`:612-613`), so the next `extend` runs at full length. If the
  allocated capacity equals `ring_frames`, `extend` reallocates on the RT thread
  exactly when output has stalled, which is the case the comment at `:610-611`
  is about. This is reasoned from the source. I did not check whether
  `VecDeque::with_capacity` gives exactly the requested capacity
  (UNVERIFIED) and did not measure it.
- `on_error` (`src/audio.rs:135-156`) calls `died` (`:178`), which formats and
  takes a mutex. Whether cpal runs the error callback on an RT thread was not
  checked (UNVERIFIED).
- No `Realtime`/`RealtimeDenied` handling exists in `src/` (grep returned
  nothing). If promotion is denied, the viewer does not say so.

**Verdict: consider** (fold into section 2).
- Change: the rtrb rewrite in section 2 removes the lock and the allocation
  path together. It needs preallocated scratch (section 2).
- Cheap, separate change: a test that asserts the ring's capacity is never
  exceeded in the input path (or a counting global allocator in a test). That
  turns the reasoning above into a check. Trade-off: a test-only allocator.
- Cheap, separate change: surface cpal's `RealtimeDenied` (the stream builder
  or error callback) in the `--stats` or dashboard line, so a run that lost RT
  scheduling says so.

---

## 5. rust Rc<RefCell> alternatives single-threaded state

**Best practice.**
- `Rc` is "A single-threaded reference-counting pointer" and is `!Send`
  (https://doc.rust-lang.org/std/rc/struct.Rc.html). The compiler stops it
  crossing threads, so it is a sound choice for UI-thread-only state.
- `RefCell::borrow_mut` "Panics if the value is currently borrowed"; use
  `try_borrow_mut` for the non-panicking form
  (https://doc.rust-lang.org/std/cell/struct.RefCell.html).
- Alternatives when shared mutable state crosses threads: a channel to one
  owner (message passing), or `Mutex`/atomics. None of those are needed for
  single-threaded state. `Cell` suits `Copy` values.

**What the codebase does.**
- Wizard: `Rc<RefCell<Wizard>>` (`src/setup.rs:70`, `src/app.rs:95`), used on
  the winit thread only (`src/app.rs:943-944`, `:997`, `:1321`, `:1472`,
  `:1534`, `:1552`). Each borrow is a short scoped statement.
- Audio controls: `Shared` holds `RefCell`s, and `Loopback(Rc<Shared>)`
  (`src/audio.rs:869-901`). `pw.rs` uses `Rc<RefCell<...>>` (`src/pw.rs:85-267`).
- The audio level worker is a thread, and it talks back through
  `LevelSlot`/`NotifySlot` mutexes, not through the `Rc` (`src/audio.rs:721-758`).
  The `Rc` graph does not cross threads.

**Verdict: keep.** The `!Send` guarantee is doing the job, and every borrow
found is short and non-reentrant. A `RefCell` double borrow would panic at
run time, so keep borrows scoped as they are. If the wizard ever calls back
into the window while borrowed, switch that call to `try_borrow_mut` and handle
the error.

---

## 6. rust graceful shutdown signal handling ctrlc

**Best practice.**
- ctrlc 3.5.2: `set_handler` "start[s] a new dedicated signal handling thread",
  so the handler runs on an ordinary thread, not in signal context. The
  `termination` feature adds SIGTERM and SIGHUP. Only one handler may be set;
  `try_set_handler` returns an error if one exists
  (https://docs.rs/ctrlc/latest/ctrlc/).
- signal-hook 0.4.6: "the set of functions one may call inside a signal handler
  is limited", and "mutexes ... and memory allocation and deallocation is _not_
  allowed" in the handler. Its `Signals` iterator and `flag` modules consume
  signals in normal threads. It notes "limited" Windows support
  (https://docs.rs/signal-hook/latest/signal_hook/).

**What the codebase does.**
- `Quit::install` calls `ctrlc::set_handler` once, at startup, before anything
  switches (`src/run.rs:256-261`; called from `src/main.rs:492`, errors exit
  with `FAILURE`).
- `Quit` is a sticky `AtomicBool` (`Relaxed`, `src/run.rs:269-276`) plus a
  per-run listener behind a mutex (`src/run.rs:247-285`). Each run installs its
  listener (`src/run.rs:149-156`), checks the flag for a request that came
  first (`:157-160`), and removes it at the end (`:232`).
- The race between a request and a listener being set is covered by the
  flag-then-lock check. Reasoned: a request that locks before the run does
  leaves the flag visible to the run's post-lock load, and one that locks after
  calls the listener. Relaxed is adequate because the flag carries no other
  data, and the listener mutex gives the ordering.

**Verdict: keep.** ctrlc's dedicated thread removes the signal-context limits
signal-hook warns about, and ctrlc covers SIGTERM/SIGHUP with the feature
already enabled (`Cargo.toml`). signal-hook's Windows support is limited, so
it is the wrong fit for the Windows build. One gap to check, not verified:
whether ctrlc's Windows path covers console-close and logoff events. Verify on
Windows before claiming it.

---

## Ranked top-5 recommendations

1. **Audio ring: replace `Mutex<Ring>` with an rtrb SPSC; move overflow discard
   to the output callback; scratch-copy for interpolation** (section 2).
   Verdict change. Removes the lock and the RT `extend` reallocation together.
   Trade-off: moderate rewrite of `play_out`, needs a real-card run
   (docs/testing.md). Gate on user go-ahead for the hardware run.
2. **Pre-size and test the audio ring path**: a test that asserts no capacity
   growth in the input path, or a counting allocator in a test (section 4).
   Cheap, and it turns the reasoning above into a check. Do this before (1) to
   confirm the problem exists. Trade-off: test-only code.
3. **Surface `RealtimeDenied`** from cpal in `--stats` or the dashboard
   (section 4). There is no handling in `src/`. Trade-off: a dashboard line.
4. **Capture writes outside the frame-slot lock; swap pointer under lock**
   (section 1, `src/run.rs:188-195`). Consider; measure first. Trade-off: one
   extra frame buffer (about 4 MB at 1080p YUYV) and a return path.
5. **Keep as is, and say so in the code**: the `Quit` race handling, the
   coalesced frame wake, the `Rc<RefCell>` wizard, the atomics-and-mutex split,
   ctrlc. Verdict keep. Only add a comment on `expect("never poisoned")` at
   `src/run.rs:192` if that section can panic.

## Unverified / open
- rossbencina.com origin page (quotes come from a mirror).
- blog.rust-lang.org/2023/01/26 (taken from search snippets and releases.rs).
- `VecDeque::with_capacity` exact-capacity behaviour (reasoned, not checked).
- cpal error-callback thread context (not checked).
- Windows ctrlc console-close coverage (not checked).
- Ring-buffer allocation and overrun behaviour: reasoned from source; not
  measured on hardware.
