//! Pass timing.
//!
//! This modules provides facilities for timing the execution of individual compilation passes.

use std::fmt;

pub use self::details::{add_to_current, take_current, PassTimes, TimingToken};

// Each pass that can be timed is predefined with the `define_passes!` macro. Each pass has a
// snake_case name and a plain text description used when printing out the timing report.
//
// This macro defines:
//
// - A C-style enum containing all the pass names and a `None` variant.
// - A usize constant with the number of defined passes.
// - A const array of pass descriptions.
// - A public function per pass used to start the timing of that pass.
macro_rules! define_passes {
    { $enum:ident, $num_passes:ident, $descriptions:ident;
      $($pass:ident: $desc:expr,)+
    } => {
        #[allow(non_camel_case_types)]
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        enum $enum { $($pass,)+ None}

        const $num_passes: usize = $enum::None as usize;

        const $descriptions: [&str; $num_passes] = [ $($desc),+ ];

        $(
            #[doc=$desc]
            pub fn $pass() -> TimingToken {
                details::start_pass($enum::$pass)
            }
        )+
    }
}

// Pass definitions.
define_passes!{
    Pass, NUM_PASSES, DESCRIPTIONS;

    process_file: "Processing test file",
    parse_text: "Parsing textual Cretonne IR",
    wasm_translate_module: "Translate WASM module",
    wasm_translate_function: "Translate WASM function",

    verifier: "Verify Cretonne IR",
    verify_cssa: "Verify CSSA",
    verify_liveness: "Verify live ranges",
    verify_locations: "Verify value locations",
    verify_flags: "Verify CPU flags",

    compile: "Compilation passes",
    flowgraph: "Control flow graph",
    domtree: "Dominator tree",
    loop_analysis: "Loop analysis",
    postopt: "Post-legalization rewriting",
    preopt: "Pre-legalization rewriting",
    dce: "Dead code elimination",
    legalize: "Legalization",
    gvn: "Global value numbering",
    licm: "Loop invariant code motion",
    unreachable_code: "Remove unreachable blocks",

    regalloc: "Register allocation",
    ra_liveness: "RA liveness analysis",
    ra_cssa: "RA coalescing CSSA",
    ra_spilling: "RA spilling",
    ra_reload: "RA reloading",
    ra_coloring: "RA coloring",

    prologue_epilogue: "Prologue/epilogue insertion",
    binemit: "Binary machine code emission",
    layout_renumber: "Layout full renumbering",
}

impl Pass {
    pub fn idx(self) -> usize {
        self as usize
    }
}

impl fmt::Display for Pass {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match DESCRIPTIONS.get(self.idx()) {
            Some(s) => f.write_str(s),
            None => f.write_str("<no pass>"),
        }
    }
}

/// Implementation details.
///
/// This whole module can be gated on a `cfg` feature to provide a dummy implementation for
/// performance-sensitive builds or restricted environments. The dummy implementation must provide
/// `TimingToken` and `PassTimings` types and a `take_current` function.
mod details {
    use super::{Pass, DESCRIPTIONS, NUM_PASSES};
    use std::cell::{Cell, RefCell};
    use std::fmt;
    use std::mem;
    use std::time::{Duration, Instant};

    /// A timing token is responsible for timing the currently running pass. Timing starts when it
    /// is created and ends when it is dropped.
    ///
    /// Multiple passes can be active at the same time, but they must be started and stopped in a
    /// LIFO fashion.
    pub struct TimingToken {
        /// Start time for this pass.
        start: Instant,

        // Pass being timed by this token.
        pass: Pass,

        // The previously active pass which will be restored when this token is dropped.
        prev: Pass,
    }

    /// Accumulated timing information for a single pass.
    #[derive(Default)]
    struct PassTime {
        /// Total time spent running this pas including children.
        total: Duration,

        /// Time spent running in child passes.
        child: Duration,
    }

    /// Accumulated timing for all passes.
    #[derive(Default)]
    pub struct PassTimes {
        pass: [PassTime; NUM_PASSES],
    }

    impl fmt::Display for PassTimes {
        fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
            writeln!(f, "======== ========  ==================================")?;
            writeln!(f, "   Total     Self  Pass")?;
            writeln!(f, "-------- --------  ----------------------------------")?;
            for (time, desc) in self.pass.iter().zip(&DESCRIPTIONS) {
                // Omit passes that haven't run.
                if time.total == Duration::default() {
                    continue;
                }

                // Write a duration as secs.milis, trailing space.
                fn fmtdur(mut dur: Duration, f: &mut fmt::Formatter) -> fmt::Result {
                    // Round to nearest ms by adding 500us.
                    dur += Duration::new(0, 500_000);
                    let ms = dur.subsec_nanos() / 1_000_000;
                    write!(f, "{:4}.{:03} ", dur.as_secs(), ms)
                }

                fmtdur(time.total, f)?;
                if let Some(s) = time.total.checked_sub(time.child) {
                    fmtdur(s, f)?;
                }
                writeln!(f, " {}", desc)?;
            }
            writeln!(f, "======== ========  ==================================")
        }
    }

    /// Information about passes in a single thread.
    thread_local!{
        static CURRENT_PASS: Cell<Pass> = Cell::new(Pass::None);
        static PASS_TIME: RefCell<PassTimes> = RefCell::new(Default::default());
    }

    /// Start timing `pass` as a child of the currently running pass, if any.
    ///
    /// This function is called by the publicly exposed pass functions.
    pub(super) fn start_pass(pass: Pass) -> TimingToken {
        let prev = CURRENT_PASS.with(|p| p.replace(pass));
        dbg!("timing: Starting {}, (during {})", pass, prev);
        TimingToken {
            start: Instant::now(),
            pass,
            prev,
        }
    }

    /// Dropping a timing token indicated the end of the pass.
    impl Drop for TimingToken {
        fn drop(&mut self) {
            let duration = self.start.elapsed();
            dbg!("timing: Ending {}", self.pass);
            let old_cur = CURRENT_PASS.with(|p| p.replace(self.prev));
            debug_assert_eq!(self.pass, old_cur, "Timing tokens dropped out of order");
            PASS_TIME.with(|rc| {
                let mut table = rc.borrow_mut();
                table.pass[self.pass.idx()].total += duration;
                if let Some(parent) = table.pass.get_mut(self.prev.idx()) {
                    parent.child += duration;
                }
            })
        }
    }

    /// Take the current accumulated pass timings and reset the timings for the current thread.
    pub fn take_current() -> PassTimes {
        PASS_TIME.with(|rc| mem::replace(&mut *rc.borrow_mut(), Default::default()))
    }

    /// Add `timings` to the accumulated timings for the current thread.
    pub fn add_to_current(times: &PassTimes) {
        PASS_TIME.with(|rc| for (a, b) in rc.borrow_mut().pass.iter_mut().zip(
            &times.pass,
        )
        {
            a.total += b.total;
            a.child += b.child;
        })
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn display() {
        assert_eq!(Pass::None.to_string(), "<no pass>");
        assert_eq!(Pass::regalloc.to_string(), "Register allocation");
    }
}
