//! `cowt diff <ID>` — changes of the isolated layer relative to the fork snapshot.

use anyhow::{bail, Result};
use cowt_core::diff::{self, Change, ChangeKind, ContentDiff};
use cowt_core::{overlay, Manifest};

use crate::backend::{default_backend, recover_stale_mount};
use crate::state::State;

pub struct DiffArgs {
    pub id: String,
    pub json: bool,
    pub content: bool,
    pub stat: bool,
}

pub fn diff_cmd(args: DiffArgs) -> Result<()> {
    let state = State::open()?;
    let dir = state.resolve(&args.id)?;
    let meta = State::load_meta(&dir)?;
    if State::running_pid(&dir).is_some() {
        bail!(
            "worktree '{}' is running; diff is available after the process exits",
            meta.id
        );
    }
    // A stale mount (crashed `cowt run`) must be restored first: on Windows a
    // dangling junction would make the target scan come up empty.
    recover_stale_mount(default_backend().as_ref(), &dir, &meta.target)?;

    let base = State::load_manifest(&dir)?;
    let upper = dir.join("upper");
    let started = std::time::Instant::now();
    let work = overlay::effective_manifest(&base, &upper)?;
    let mut changes = diff::diff(&base, &work);

    if args.content {
        // Line/key-level details need the base file bodies. Bodies are not
        // snapshotted at fork time (metadata only), so the base side is read
        // from the live target — but only when the live file is provably
        // identical to the fork-time snapshot (hash match).
        let current = Manifest::rescan(&meta.target, &base)
            .map(|s| s.manifest)
            .ok();
        for ch in changes.iter_mut() {
            if ch.kind != ChangeKind::Modified {
                continue;
            }
            let base_body_valid = current
                .as_ref()
                .and_then(|c| c.get(&ch.path))
                .zip(base.get(&ch.path))
                .map(|(c, b)| c.content_eq(b))
                .unwrap_or(false);
            if !base_body_valid {
                continue; // host moved since fork; structural diff only
            }
            let old = meta.target.join(&ch.path);
            let new = upper.join(&ch.path);
            let mut one = vec![ch.clone()];
            diff::enrich(&meta.target, &upper, &mut one);
            ch.detail = one.pop().and_then(|c| c.detail);
            let _ = (old, new);
        }
    }
    let elapsed = started.elapsed();

    if args.json {
        println!("{}", serde_json::to_string_pretty(&changes)?);
        return Ok(());
    }

    print_human(&changes, args.stat);
    eprintln!(
        "cowt: diff computed in {:.0}ms ({} change(s))",
        elapsed.as_secs_f64() * 1000.0,
        changes.len()
    );
    Ok(())
}

fn print_human(changes: &[Change], stat: bool) {
    if changes.is_empty() {
        println!("no changes");
        return;
    }
    let (mut a, mut m, mut d) = (0usize, 0usize, 0usize);
    for ch in changes {
        let tag = match ch.kind {
            ChangeKind::Added => {
                a += 1;
                "A"
            }
            ChangeKind::Modified => {
                m += 1;
                "M"
            }
            ChangeKind::Deleted => {
                d += 1;
                "D"
            }
        };
        println!("{tag}  {}", ch.path.display());
        if !stat {
            match &ch.detail {
                Some(ContentDiff::Text { unified }) => {
                    for line in unified.lines() {
                        println!("    {line}");
                    }
                }
                Some(ContentDiff::Keys { changes }) => {
                    for k in changes {
                        let t = match k.kind {
                            ChangeKind::Added => "+",
                            ChangeKind::Modified => "~",
                            ChangeKind::Deleted => "-",
                        };
                        match (&k.old, &k.new) {
                            (Some(o), Some(n)) => println!("    {t} {}: {o} -> {n}", k.key),
                            (None, Some(n)) => println!("    {t} {}: {n}", k.key),
                            (Some(o), None) => println!("    {t} {}: (was {o})", k.key),
                            _ => {}
                        }
                    }
                }
                Some(ContentDiff::Binary) => println!("    (binary content changed)"),
                None => {}
            }
        }
    }
    println!("summary: {a} added, {m} modified, {d} deleted");
}
