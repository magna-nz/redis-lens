//! 1 Overview: what `App` keeps for the Overview area, and "Analyze the keys?" (`r`).
//!
//! Owner: the Overview agent fills this out; the navigation core only put the pieces in
//! place. The live figures at the top of Overview are the Server view state's Overview
//! poll ([`super::server::ServerState::overview`]): `Screen::Overview` is owned by
//! [`super::Owner::Server`], so that poll and the cluster per-shard poll run while
//! Overview is on screen, as Server › Overview's did. Drawing is in `ui/overview.rs`.
//!
//! # Analyzing
//!
//! A connect reads no key. `r` on Overview, Browse › Keys and the Analyze panels opens
//! [`AnalyzeDialog`]: Quick ([`Depth::Quick`], 10,000 keys, the default) or Deep
//! ([`Depth::Deep`], 100,000 keys), each with [`estimate`]'s rough cost. `enter` starts
//! it as a new run of today's scan machinery with `ScanOptions::sample_limit` set to
//! [`Depth::sample_limit`] (`Effect::Connect` with `sample`), keeping the old results on
//! screen meanwhile as a rescan did. `esc` while it runs cancels it
//! (`Effect::CancelAnalysis`); a later change turns that into stop-and-keep.

use crate::model::Profile;
use crate::scan::ScanOptions;

/// How many keys an analysis samples.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Depth {
    /// 10,000 keys: the default.
    #[default]
    Quick,
    /// 100,000 keys: finds rare patterns and rare big keys.
    Deep,
}

impl Depth {
    pub const ALL: [Depth; 2] = [Depth::Quick, Depth::Deep];

    /// `ScanOptions::sample_limit` for this depth.
    pub fn sample_limit(self) -> u64 {
        match self {
            Self::Quick => 10_000,
            Self::Deep => 100_000,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Quick => "Quick",
            Self::Deep => "Deep",
        }
    }
}

/// What an analysis costs, roughly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Estimate {
    /// Keys sampled.
    pub keys: u64,
    /// Commands sent, about.
    pub commands: u64,
    /// The profile's command budget, per second; `None` when it has none (local).
    pub per_sec: Option<u32>,
    /// Seconds it takes at that budget, about; `None` without a budget.
    pub secs: Option<u64>,
}

/// Commands per sampled key in [`estimate`]: `TYPE`, `PTTL`, `MEMORY USAGE` and
/// `OBJECT FREQ`, pipelined.
pub const COMMANDS_PER_KEY: u64 = 4;

/// A rough cost of a `depth` analysis of `profile`: sample × [`COMMANDS_PER_KEY`]
/// commands, at the profile's command budget (`ScanOptions::for_profile`).
///
/// The one place the dialog, Overview and the "waits for an analysis" panels take
/// their figures from: swap in the scan engine's own estimate here.
pub fn estimate(depth: Depth, profile: &Profile) -> Estimate {
    let keys = depth.sample_limit();
    let commands = keys * COMMANDS_PER_KEY;
    let per_sec = ScanOptions::for_profile(profile).max_commands_per_sec;
    let secs = per_sec
        .filter(|r| *r > 0)
        .map(|r| commands.div_ceil(u64::from(r)));
    Estimate {
        keys,
        commands,
        per_sec,
        secs,
    }
}

/// `about 8 s`, `about 2 min`, or how fast without a budget.
pub fn about_time(e: &Estimate) -> String {
    match e.secs {
        Some(s) if s < 120 => format!("about {s} s"),
        Some(s) => format!("about {} min", s.div_ceil(60)),
        None => "as fast as the server answers".to_string(),
    }
}

/// "Analyze the keys?": open while `App::analyze` is `Some`. Every key goes to it:
/// `↑↓` (and `j` `k`) pick, `enter` starts, `esc` cancels.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AnalyzeDialog {
    pub pick: Depth,
}

/// Overview's own state; reset on every connect.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OverviewState {
    /// The selected finding under "Keys" (`↑↓`; `enter` goes there).
    pub sel: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Auth, Endpoint, Env};

    fn profile(env: Env) -> Profile {
        Profile {
            name: "p".into(),
            env,
            endpoint: Endpoint {
                host: "h".into(),
                port: 6379,
                tls: false,
            },
            db: 0,
            auth: Auth::None,
            read_only: None,
            prefer_replica: false,
            provider: None,
            token_command: None,
            ssh: None,
            sentinel: None,
            tags: Vec::new(),
        }
    }

    #[test]
    fn quick_is_the_default_and_samples_ten_thousand() {
        assert_eq!(AnalyzeDialog::default().pick, Depth::Quick);
        assert_eq!(Depth::Quick.sample_limit(), 10_000);
        assert_eq!(Depth::Deep.sample_limit(), 100_000);
    }

    #[test]
    fn the_estimate_is_four_commands_a_key_at_the_budget() {
        let e = estimate(Depth::Quick, &profile(Env::Prod));
        assert_eq!(e.commands, 40_000);
        assert_eq!(e.per_sec, Some(5_000));
        assert_eq!(e.secs, Some(8));
        assert_eq!(about_time(&e), "about 8 s");
        let e = estimate(Depth::Deep, &profile(Env::Prod));
        assert_eq!(e.secs, Some(80));
        // Local profiles have no budget.
        let e = estimate(Depth::Deep, &profile(Env::Local));
        assert_eq!((e.per_sec, e.secs), (None, None));
        assert_eq!(about_time(&e), "as fast as the server answers");
    }
}
