//! `redis-lens snapshots list|prune <profile>`: a profile's saved snapshots.
//!
//! The logic lives in [`entries`], [`render_list`] and [`prune`] so tests can run it
//! against a temp snapshot directory; [`run`] wires it to the data directory and the
//! terminal. Only files in redis-lens's own snapshot directory are read or deleted, and
//! never a server.

use std::path::{Path, PathBuf};

use anyhow::Context;

use super::SnapshotsCommand;
use crate::format;
use crate::snapshot::{self, Retention};

/// One row of `snapshots list`.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Entry {
    pub file: String,
    pub path: PathBuf,
    pub taken_at_unix: i64,
    /// `2026-11-11 14:02:11 UTC`.
    pub taken_at: String,
    /// `None` when the file could not be read in full.
    pub patterns: Option<usize>,
    pub sampled_keys: Option<u64>,
    pub complete: Option<bool>,
    pub match_glob: Option<String>,
}

/// `profile`'s snapshots in `dir`, newest first, with what each one holds.
pub fn entries(dir: &Path, profile: &str) -> anyhow::Result<Vec<Entry>> {
    Ok(snapshot::list(dir, profile)?
        .into_iter()
        .map(|info| {
            let report = snapshot::load(&info.path).ok();
            Entry {
                file: info
                    .path
                    .file_name()
                    .map_or_else(String::new, |n| n.to_string_lossy().into_owned()),
                taken_at: snapshot::format_utc(info.taken_at_unix),
                taken_at_unix: info.taken_at_unix,
                patterns: report.as_ref().map(|r| r.patterns.len()),
                sampled_keys: report.as_ref().map(|r| r.sampled_keys),
                complete: report.as_ref().map(|r| r.complete),
                match_glob: report.and_then(|r| r.match_glob),
                path: info.path,
            }
        })
        .collect())
}

fn table(heads: &[&str], rows: &[Vec<String>], right: &[bool]) -> Vec<String> {
    let mut w: Vec<usize> = heads.iter().map(|h| h.chars().count()).collect();
    for r in rows {
        for (i, c) in r.iter().enumerate() {
            w[i] = w[i].max(c.chars().count());
        }
    }
    let line = |cells: Vec<&str>| -> String {
        cells
            .iter()
            .enumerate()
            .map(|(i, c)| {
                if right[i] {
                    format!("{c:>w$}", w = w[i])
                } else {
                    format!("{c:<w$}", w = w[i])
                }
            })
            .collect::<Vec<_>>()
            .join("  ")
            .trim_end()
            .to_string()
    };
    let mut out = vec![line(heads.to_vec())];
    out.extend(
        rows.iter()
            .map(|r| line(r.iter().map(String::as_str).collect())),
    );
    out
}

/// `snapshots list` as text.
pub fn render_list(profile: &str, dir: &Path, entries: &[Entry]) -> String {
    let pdir = snapshot::profile_dir_path(dir, profile);
    if entries.is_empty() {
        return format!(
            "No snapshots for {profile} in {}.\nSave one with `redis-lens scan {profile} --save`.",
            pdir.display()
        );
    }
    let dash = || "—".to_string();
    let rows: Vec<Vec<String>> = entries
        .iter()
        .map(|e| {
            vec![
                e.taken_at.clone(),
                e.file.clone(),
                e.patterns.map_or_else(dash, |n| n.to_string()),
                e.sampled_keys.map_or_else(dash, format::grouped),
                match e.complete {
                    Some(true) => "yes".to_string(),
                    Some(false) => "no".to_string(),
                    None => "unreadable".to_string(),
                },
                e.match_glob.clone().unwrap_or_else(dash),
            ]
        })
        .collect();
    let mut lines = vec![
        format!(
            "{} snapshot(s) of {profile}, newest first, in {}",
            entries.len(),
            pdir.display()
        ),
        String::new(),
    ];
    lines.extend(table(
        &["TAKEN", "FILE", "PATTERNS", "SAMPLED", "COMPLETE", "MATCH"],
        &rows,
        &[false, false, true, true, false, false],
    ));
    lines.join("\n")
}

/// `snapshots prune`: what was (with `yes`) or would be (without) deleted, as text.
pub fn prune(
    dir: &Path,
    profile: &str,
    retention: Retention,
    yes: bool,
    now_unix: i64,
) -> anyhow::Result<String> {
    if retention == Retention::default() {
        anyhow::bail!(
            "prune needs --keep N or --keep-days D (or both); see `redis-lens snapshots prune --help`"
        );
    }
    let total = snapshot::list(dir, profile)?.len();
    if total == 0 {
        return Ok(format!("No snapshots for {profile}; nothing to delete."));
    }
    let doomed: Vec<(i64, PathBuf)> = if yes {
        snapshot::apply_retention(dir, profile, retention, now_unix)?
            .into_iter()
            .map(|p| (time_of(&p), p))
            .collect()
    } else {
        snapshot::retention_plan(dir, profile, retention, now_unix)?
            .into_iter()
            .map(|i| (i.taken_at_unix, i.path))
            .collect()
    };
    if doomed.is_empty() {
        return Ok(format!(
            "Nothing to delete: all {total} snapshot(s) of {profile} are kept."
        ));
    }
    let kept = total - doomed.len();
    let mut lines = vec![if yes {
        format!(
            "Deleted {} snapshot(s) of {profile}; {kept} kept:",
            doomed.len()
        )
    } else {
        format!(
            "Would delete {} snapshot(s) of {profile}; {kept} kept:",
            doomed.len()
        )
    }];
    for (t, path) in &doomed {
        lines.push(format!(
            "  {}  {}",
            snapshot::format_utc(*t),
            path.display()
        ));
    }
    if !yes {
        lines.push("Nothing was deleted. Run again with --yes to delete them.".to_string());
    }
    Ok(lines.join("\n"))
}

/// The time in a snapshot file name, for display; 0 if the name is not one.
fn time_of(path: &Path) -> i64 {
    path.file_stem()
        .and_then(|s| s.to_str())
        .and_then(snapshot::parse_timestamp)
        .unwrap_or(0)
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

pub async fn run(_config: Option<PathBuf>, cmd: SnapshotsCommand) -> anyhow::Result<()> {
    let dir = snapshot::default_dir()?;
    match cmd {
        SnapshotsCommand::List { profile, json } => {
            let entries = entries(&dir, &profile)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&entries)
                        .context("could not serialize the list")?
                );
            } else {
                println!("{}", render_list(&profile, &dir, &entries));
            }
        }
        SnapshotsCommand::Prune {
            profile,
            keep,
            keep_days,
            yes,
        } => {
            let retention = Retention { keep, keep_days };
            println!("{}", prune(&dir, &profile, retention, yes, now_unix())?);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ScanReport, ServerFlavor, ServerInfo, ServerMode};

    const DAY: i64 = 86_400;
    const NOW: i64 = 1_794_319_331;

    fn temp_dir(tag: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("redis-lens-snapcli-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn report(profile: &str, at: i64, complete: bool, glob: Option<&str>) -> ScanReport {
        ScanReport {
            profile: profile.into(),
            taken_at_unix: at,
            server: ServerInfo {
                flavor: ServerFlavor::Redis,
                version: "7".into(),
                mode: ServerMode::Standalone,
                used_memory: 1,
                maxmemory: 0,
                maxmemory_policy: "noeviction".into(),
                total_keys: 0,
            },
            sampled_keys: 12_400,
            patterns: vec![],
            sizes_available: true,
            hot_keys: vec![],
            complete,
            match_glob: glob.map(str::to_string),
            server_context: None,
            key_sizes: None,
        }
    }

    /// Three snapshots of `p`, a day apart, the newest at NOW; one used a glob.
    fn three(tag: &str) -> PathBuf {
        let dir = temp_dir(tag);
        snapshot::save(&dir, &report("p", NOW, true, None)).unwrap();
        snapshot::save(&dir, &report("p", NOW - DAY, false, Some("user:*"))).unwrap();
        snapshot::save(&dir, &report("p", NOW - 2 * DAY, false, None)).unwrap();
        snapshot::save(&dir, &report("other", NOW, true, None)).unwrap();
        dir
    }

    #[test]
    fn list_newest_first_with_details() {
        let dir = three("list");
        let e = entries(&dir, "p").unwrap();
        let t = render_list("p", &dir, &e);
        println!("{t}");
        let lines: Vec<&str> = t.lines().collect();
        assert!(lines[0].starts_with("3 snapshot(s) of p, newest first, in "));
        assert!(lines[0].ends_with(&snapshot::profile_dir_path(&dir, "p").display().to_string()));
        assert_eq!(
            lines[2],
            "TAKEN                    FILE                   PATTERNS  SAMPLED  COMPLETE  MATCH"
        );
        assert_eq!(
            lines[3],
            "2026-11-10 14:02:11 UTC  20261110T140211Z.json         0   12,400  yes       —"
        );
        assert_eq!(
            lines[4],
            "2026-11-09 14:02:11 UTC  20261109T140211Z.json         0   12,400  no        user:*"
        );
        assert!(lines[5].starts_with("2026-11-08 14:02:11 UTC"));
        assert_eq!(lines.len(), 6);
        let json = serde_json::to_value(&e).unwrap();
        assert_eq!(json[1]["match_glob"], "user:*");
        assert_eq!(json[1]["complete"], false);
        assert_eq!(json[0]["taken_at"], "2026-11-10 14:02:11 UTC");
        assert_eq!(json[0]["file"], "20261110T140211Z.json");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_of_nothing_says_how_to_save() {
        let dir = temp_dir("list-empty");
        let t = render_list("p", &dir, &entries(&dir, "p").unwrap());
        assert!(t.starts_with("No snapshots for p in "), "{t}");
        assert!(
            t.ends_with("Save one with `redis-lens scan p --save`."),
            "{t}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_marks_unreadable_files() {
        let dir = three("list-bad");
        // A header that parses but a body that doesn't.
        let path = snapshot::profile_dir_path(&dir, "p")
            .join(format!("{}.json", snapshot::timestamp_name(NOW - 3 * DAY)));
        std::fs::write(&path, r#"{"profile":"p"}"#).unwrap();
        let e = entries(&dir, "p").unwrap();
        assert_eq!(e.len(), 4);
        assert_eq!(e[3].patterns, None);
        let t = render_list("p", &dir, &e);
        assert!(t.lines().last().unwrap().contains("unreadable"), "{t}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prune_without_yes_only_lists() {
        let dir = three("prune-dry");
        let r = Retention {
            keep: Some(1),
            keep_days: None,
        };
        let t = prune(&dir, "p", r, false, NOW).unwrap();
        println!("{t}");
        let lines: Vec<&str> = t.lines().collect();
        assert_eq!(lines[0], "Would delete 2 snapshot(s) of p; 1 kept:");
        assert!(lines[1].starts_with("  2026-11-09 14:02:11 UTC  "), "{t}");
        assert!(lines[1].ends_with("20261109T140211Z.json"), "{t}");
        assert!(lines[2].starts_with("  2026-11-08 14:02:11 UTC  "), "{t}");
        assert_eq!(
            lines[3],
            "Nothing was deleted. Run again with --yes to delete them."
        );
        assert_eq!(snapshot::list(&dir, "p").unwrap().len(), 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prune_with_yes_deletes_and_reports() {
        let dir = three("prune-yes");
        let r = Retention {
            keep: None,
            keep_days: Some(1),
        };
        let t = prune(&dir, "p", r, true, NOW).unwrap();
        let lines: Vec<&str> = t.lines().collect();
        assert_eq!(lines[0], "Deleted 1 snapshot(s) of p; 2 kept:");
        assert!(lines[1].starts_with("  2026-11-08 14:02:11 UTC  "), "{t}");
        assert_eq!(lines.len(), 2);
        assert_eq!(snapshot::list(&dir, "p").unwrap().len(), 2);
        // Another profile is untouched.
        assert_eq!(snapshot::list(&dir, "other").unwrap().len(), 1);
        // Again: nothing left to delete.
        assert_eq!(
            prune(&dir, "p", r, true, NOW).unwrap(),
            "Nothing to delete: all 2 snapshot(s) of p are kept."
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prune_needs_a_limit() {
        let dir = three("prune-usage");
        let err = prune(&dir, "p", Retention::default(), true, NOW).unwrap_err();
        assert!(
            err.to_string()
                .starts_with("prune needs --keep N or --keep-days D"),
            "{err}"
        );
        assert_eq!(snapshot::list(&dir, "p").unwrap().len(), 3);
        let r = Retention {
            keep: Some(1),
            keep_days: None,
        };
        assert_eq!(
            prune(&dir, "nobody", r, true, NOW).unwrap(),
            "No snapshots for nobody; nothing to delete."
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prune_flags_parse() {
        use clap::Parser;
        let cmd = |argv: &[&str]| match crate::cli::Cli::try_parse_from(argv).unwrap().command {
            Some(crate::cli::Command::Snapshots(c)) => c,
            other => panic!("not snapshots: {other:?}"),
        };
        match cmd(&[
            "redis-lens",
            "snapshots",
            "prune",
            "p",
            "--keep",
            "3",
            "--keep-days",
            "30",
            "--yes",
        ]) {
            SnapshotsCommand::Prune {
                profile,
                keep,
                keep_days,
                yes,
            } => assert_eq!(
                (profile.as_str(), keep, keep_days, yes),
                ("p", Some(3), Some(30), true)
            ),
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            cmd(&["redis-lens", "snapshots", "list", "p", "--json"]),
            SnapshotsCommand::List { json: true, .. }
        ));
    }
}
