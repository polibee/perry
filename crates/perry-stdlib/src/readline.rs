//! readline module for Perry — Phase 1 of #347
//!
//! Provides line-buffered stdin reading via `readline.createInterface`:
//!   const rl = readline.createInterface({ input: process.stdin, output: process.stdout });
//!   rl.question("name? ", (answer) => { ... });
//!   rl.on("line", (line) => { ... });
//!   rl.on("close", () => { ... });
//!   rl.close();
//!
//! Architecture mirrors `worker_threads.rs::start_stdin_reader` — a single
//! background thread reads lines from stdin and queues them; the main event
//! loop drains the queue on every tick via `js_readline_process_pending`,
//! dispatching to the registered question/line/close callbacks.
//!
//! Phases 2 (raw mode) and 3 (resize/isatty) layer on top of this.

use std::cell::RefCell;
use std::io::{self, BufRead, Write};

use perry_runtime::closure::{js_closure_call0, js_closure_call1, ClosureHeader};
use perry_runtime::string::{js_string_from_bytes, StringHeader};
use perry_runtime::value::JSValue;

/// Singleton handle for the readline interface. createInterface always
/// returns this — Node also tolerates multiple createInterface calls on
/// the same input, but for v1 we treat it as a process-wide singleton
/// since stdin can only have one consumer at a time anyway.
const READLINE_HANDLE: i64 = 1;

thread_local! {
    /// One-shot callback registered by `rl.question(prompt, cb)`. Cleared
    /// when fired so a second `question` call doesn't double-fire on the
    /// next line.
    static QUESTION_CALLBACK: RefCell<Option<i64>> = const { RefCell::new(None) };
    /// Persistent callback registered by `rl.on('line', cb)`.
    static LINE_CALLBACK: RefCell<Option<i64>> = const { RefCell::new(None) };
    /// Persistent callback registered by `rl.on('close', cb)`. Fired once
    /// when the interface closes (EOF or explicit `rl.close()`).
    static CLOSE_CALLBACK: RefCell<Option<i64>> = const { RefCell::new(None) };
    /// Lines waiting for the main thread to dispatch.
    static PENDING_LINES: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    /// Whether the background reader thread has been spawned.
    static READER_STARTED: RefCell<bool> = const { RefCell::new(false) };
    /// Set when stdin returns EOF or `rl.close()` is called. Drives the
    /// has_pending check so the event loop knows it can exit.
    static EOF_REACHED: RefCell<bool> = const { RefCell::new(false) };
    /// Whether the close callback has already fired (so we don't fire it
    /// twice when EOF and explicit close coincide).
    static CLOSE_FIRED: RefCell<bool> = const { RefCell::new(false) };
}

/// Spawn the background line-reader if it isn't already running. Idempotent.
fn ensure_reader_started() {
    let already = READER_STARTED.with(|s| {
        let was = *s.borrow();
        *s.borrow_mut() = true;
        was
    });
    if already {
        return;
    }
    std::thread::spawn(move || {
        let stdin = io::stdin();
        let reader = stdin.lock();
        for line in reader.lines() {
            match line {
                Ok(line) => {
                    PENDING_LINES.with(|q| q.borrow_mut().push(line));
                }
                Err(_) => break,
            }
        }
        EOF_REACHED.with(|eof| *eof.borrow_mut() = true);
    });
}

/// readline.createInterface(opts) — returns a NaN-boxed POINTER handle
/// pointing at the singleton interface. The opts argument is accepted
/// for shape compatibility with Node but currently ignored (input is
/// always process.stdin, output is always process.stdout).
#[no_mangle]
pub extern "C" fn js_readline_create_interface(_opts: f64) -> i64 {
    // Register the stdlib pump with perry-runtime so the event loop
    // ticks even when the program never `await`s. Without this, a
    // program that only uses readline (no setTimeout / no fetch / no
    // promise) exits immediately after `main` returns and the close
    // callback never fires.
    crate::common::async_bridge::ensure_pump_registered();
    ensure_reader_started();
    READLINE_HANDLE
}

/// rl.question(prompt, callback) — write `prompt` to stdout (no newline,
/// matching Node) and register `callback` as a one-shot to fire with the
/// next line read. If a previous question is still pending, it is
/// silently replaced (matches Node's behavior of overwriting).
#[no_mangle]
pub extern "C" fn js_readline_question(
    _handle: i64,
    prompt_ptr: *const StringHeader,
    callback: i64,
) -> f64 {
    if !prompt_ptr.is_null() {
        unsafe {
            let len = (*prompt_ptr).byte_len as usize;
            let data = (prompt_ptr as *const u8).add(std::mem::size_of::<StringHeader>());
            let bytes = std::slice::from_raw_parts(data, len);
            let stdout = io::stdout();
            let mut h = stdout.lock();
            let _ = h.write_all(bytes);
            let _ = h.flush();
        }
    }
    QUESTION_CALLBACK.with(|cb| *cb.borrow_mut() = Some(callback));
    ensure_reader_started();
    f64::from_bits(JSValue::undefined().bits())
}

/// rl.on(event, callback) — register a persistent callback for the
/// `'line'` or `'close'` event. Other event names are silently ignored
/// for now (Node has more — 'pause', 'resume', 'SIGINT', 'history' —
/// but they aren't part of this phase's surface).
#[no_mangle]
pub extern "C" fn js_readline_on(
    _handle: i64,
    event_ptr: *const StringHeader,
    callback: i64,
) -> f64 {
    if event_ptr.is_null() {
        return f64::from_bits(JSValue::undefined().bits());
    }
    let event = unsafe {
        let len = (*event_ptr).byte_len as usize;
        let data = (event_ptr as *const u8).add(std::mem::size_of::<StringHeader>());
        let slice = std::slice::from_raw_parts(data, len);
        std::str::from_utf8(slice).unwrap_or("")
    };
    match event {
        "line" => {
            LINE_CALLBACK.with(|cb| *cb.borrow_mut() = Some(callback));
            ensure_reader_started();
        }
        "close" => {
            CLOSE_CALLBACK.with(|cb| *cb.borrow_mut() = Some(callback));
        }
        _ => {}
    }
    f64::from_bits(JSValue::undefined().bits())
}

/// rl.close() — mark the interface as closed and synchronously fire
/// the 'close' callback (if registered). Node's `Interface.close()`
/// emits 'close' synchronously inside the call, so a program that does
/// `rl.on('close', () => log('A')); rl.close(); log('B')` prints "A"
/// before "B". We match that ordering here rather than deferring to the
/// next event-loop tick.
#[no_mangle]
pub extern "C" fn js_readline_close(_handle: i64) -> f64 {
    EOF_REACHED.with(|eof| *eof.borrow_mut() = true);
    let already = CLOSE_FIRED.with(|f| {
        let was = *f.borrow();
        *f.borrow_mut() = true;
        was
    });
    if !already {
        let cb = CLOSE_CALLBACK.with(|c| c.borrow_mut().take());
        if let Some(cb_i64) = cb {
            let closure = cb_i64 as *const ClosureHeader;
            unsafe { js_closure_call0(closure) };
        }
    }
    f64::from_bits(JSValue::undefined().bits())
}

/// Drain the line queue and dispatch callbacks. Called from the
/// async-bridge tick on every event-loop iteration. Returns the number
/// of callbacks fired (zero counts as "no work this tick").
#[no_mangle]
pub extern "C" fn js_readline_process_pending() -> i32 {
    let mut fired: i32 = 0;

    let lines: Vec<String> = PENDING_LINES.with(|q| q.borrow_mut().drain(..).collect());
    for line in lines {
        let str_ptr = js_string_from_bytes(line.as_ptr(), line.len() as u32);
        let arg = JSValue::string_ptr(str_ptr).bits();
        let arg_f = f64::from_bits(arg);
        // question() takes precedence — it's a one-shot consuming the
        // first available line, so a `question` registered between two
        // pending `line` events still fires on the very next read.
        let q_cb = QUESTION_CALLBACK.with(|cb| cb.borrow_mut().take());
        if let Some(cb_i64) = q_cb {
            let closure = cb_i64 as *const ClosureHeader;
            unsafe { js_closure_call1(closure, arg_f) };
            fired += 1;
            continue;
        }
        let line_cb = LINE_CALLBACK.with(|cb| *cb.borrow());
        if let Some(cb_i64) = line_cb {
            let closure = cb_i64 as *const ClosureHeader;
            unsafe { js_closure_call1(closure, arg_f) };
            fired += 1;
        }
    }

    let eof = EOF_REACHED.with(|e| *e.borrow());
    if eof {
        let already = CLOSE_FIRED.with(|f| {
            let was = *f.borrow();
            *f.borrow_mut() = true;
            was
        });
        if !already {
            let cb = CLOSE_CALLBACK.with(|c| c.borrow_mut().take());
            if let Some(cb_i64) = cb {
                let closure = cb_i64 as *const ClosureHeader;
                unsafe { js_closure_call0(closure) };
                fired += 1;
            }
        }
    }
    fired
}

/// Whether readline has any active state that requires the event loop
/// to keep running. Returns 1 while the reader is started and EOF has
/// not yet been observed (or while there are queued lines to drain).
#[no_mangle]
pub extern "C" fn js_readline_has_active() -> i32 {
    let started = READER_STARTED.with(|s| *s.borrow());
    let eof = EOF_REACHED.with(|e| *e.borrow());
    let has_lines = PENDING_LINES.with(|q| !q.borrow().is_empty());
    let has_close_cb =
        !CLOSE_FIRED.with(|f| *f.borrow()) && CLOSE_CALLBACK.with(|c| c.borrow().is_some());
    if has_lines || has_close_cb || (started && !eof) {
        1
    } else {
        0
    }
}

/// Test-only helper: bypass the stdin reader and inject a line into the
/// queue. Used by the unit tests to exercise the dispatch path without
/// requiring an interactive terminal.
#[doc(hidden)]
#[cfg(test)]
fn test_inject_line(line: &str) {
    PENDING_LINES.with(|q| q.borrow_mut().push(line.to_string()));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reset() {
        QUESTION_CALLBACK.with(|c| *c.borrow_mut() = None);
        LINE_CALLBACK.with(|c| *c.borrow_mut() = None);
        CLOSE_CALLBACK.with(|c| *c.borrow_mut() = None);
        PENDING_LINES.with(|q| q.borrow_mut().clear());
        EOF_REACHED.with(|e| *e.borrow_mut() = false);
        CLOSE_FIRED.with(|f| *f.borrow_mut() = false);
        // Don't touch READER_STARTED — once the thread is spawned in a
        // test process we can't unspawn it; the per-test isolation comes
        // from clearing the queue + callbacks above.
    }

    #[test]
    fn close_without_callbacks_is_noop() {
        reset();
        let h = js_readline_create_interface(0.0);
        assert_eq!(h, READLINE_HANDLE);
        js_readline_close(h);
        // EOF flag set but no callback registered → process_pending returns 0
        assert_eq!(js_readline_process_pending(), 0);
        // Second drain still 0 — close already fired its no-op pass
        assert_eq!(js_readline_process_pending(), 0);
    }

    #[test]
    fn injected_line_drains_via_test_helper() {
        reset();
        test_inject_line("hello");
        // No callback registered → drain consumes the line silently and
        // reports 0 callbacks fired (the line is dropped).
        assert_eq!(js_readline_process_pending(), 0);
        // Queue is now empty.
        let still = PENDING_LINES.with(|q| q.borrow().len());
        assert_eq!(still, 0);
    }

    #[test]
    fn has_active_reflects_state() {
        reset();
        // Pre-create: reader hasn't started → no active state.
        // (READER_STARTED may be true from an earlier test; the EOF flag
        // is what gates the active check most commonly. After EOF, with
        // no pending lines and no close callback waiting, has_active=0.)
        EOF_REACHED.with(|e| *e.borrow_mut() = true);
        CLOSE_FIRED.with(|f| *f.borrow_mut() = true);
        assert_eq!(js_readline_has_active(), 0);
        // Inject a pending line → has_active flips to 1.
        test_inject_line("x");
        assert_eq!(js_readline_has_active(), 1);
        PENDING_LINES.with(|q| q.borrow_mut().clear());
        assert_eq!(js_readline_has_active(), 0);
    }
}
