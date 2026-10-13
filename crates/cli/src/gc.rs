//! `cave gc`: shows disk usage and what can be reclaimed, then asks before
//! deleting it (docs/cli.md, "cave gc").

use std::io::{self, BufRead, IsTerminal};
use std::path::Path;

use cave_vm_proto::daemon::{Reply, Request, Usage};

use crate::client::Connection;
use crate::error::{CliError, Result};

fn gigabytes(bytes: u64) -> String {
    format!("{:.1} GB", bytes as f64 / f64::from(1 << 30))
}

fn call(apply: bool) -> Result<Usage> {
    match Connection::call(&Request::Gc { apply })? {
        Reply::Usage(usage) => Ok(usage),
        _ => Err(CliError::new("the daemon sent an unexpected reply to gc")),
    }
}

fn reclaimable(usage: &Usage) -> u64 {
    usage.reclaimable.iter().map(|r| r.size).sum()
}

/// The usage table, one row for the images, one per project and one for
/// what can be reclaimed.
fn table(usage: &Usage) -> String {
    let mut rows: Vec<(String, u64, String)> = Vec::new();
    if !usage.images.is_empty() {
        let detail: Vec<String> = usage
            .images
            .iter()
            .map(|i| {
                if i.in_use {
                    format!("{} (in use)", i.name)
                } else {
                    format!("{} (unused, {})", i.name, gigabytes(i.size))
                }
            })
            .collect();
        rows.push((
            "Images".into(),
            usage.images.iter().map(|i| i.size).sum(),
            detail.join(", "),
        ));
    }
    for p in &usage.projects {
        let name = Path::new(&p.project)
            .file_name()
            .map_or_else(|| p.project.clone(), |n| n.to_string_lossy().into_owned());
        let mut detail = vec![format!("disk {}", gigabytes(p.disk))];
        if p.snapshots > 0 {
            detail.push(format!("snapshots {}", gigabytes(p.snapshots)));
        }
        if p.forks > 0 {
            detail.push(format!("forks {}", gigabytes(p.forks)));
        }
        rows.push((name, p.disk + p.snapshots + p.forks, detail.join(", ")));
    }
    if !usage.reclaimable.is_empty() {
        let detail: Vec<String> = usage
            .reclaimable
            .iter()
            .map(|r| format!("{} {}", r.what, gigabytes(r.size)))
            .collect();
        rows.push(("Reclaimable".into(), reclaimable(usage), detail.join(", ")));
    }
    let width = rows.iter().map(|r| r.0.len()).max().unwrap_or(0).max(12) + 2;
    rows.iter()
        .map(|(name, size, detail)| format!("{name:<width$}{:<9}{detail}\n", gigabytes(*size)))
        .collect()
}

fn print_json(usage: &Usage) -> Result<()> {
    println!(
        "{}",
        serde_json::to_string_pretty(usage).map_err(|e| CliError::new(e.to_string()))?
    );
    Ok(())
}

/// `cave gc`.
pub fn run(yes: bool, json: bool) -> Result<()> {
    let usage = call(false)?;
    if json && !yes {
        return print_json(&usage);
    }
    if !json {
        print!("{}", table(&usage));
    }
    if usage.reclaimable.is_empty() {
        if json {
            return print_json(&usage);
        }
        println!("Nothing to reclaim");
        return Ok(());
    }
    if !yes {
        if !io::stdin().is_terminal() {
            return Err(CliError::new("cave gc needs a terminal to ask first")
                .fix("Add --yes to delete without asking, for scripts"));
        }
        eprint!("Delete {}? [y/N] ", gigabytes(reclaimable(&usage)));
        let mut answer = String::new();
        io::stdin().lock().read_line(&mut answer)?;
        if !matches!(answer.trim(), "y" | "Y" | "yes" | "Yes") {
            println!("Nothing was deleted");
            return Ok(());
        }
    }
    let done = call(true)?;
    if json {
        return print_json(&done);
    }
    println!("Deleted {}", gigabytes(done.reclaimed.unwrap_or(0)));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cave_vm_proto::daemon::{ImageUsage, ProjectUsage, Reclaimable};

    const GB: u64 = 1 << 30;

    #[test]
    fn the_table_matches_the_docs() {
        let usage = Usage {
            images: vec![
                ImageUsage {
                    name: "debian".into(),
                    digest: "sha256:aa".into(),
                    size: GB / 5,
                    in_use: true,
                },
                ImageUsage {
                    name: "ubuntu".into(),
                    digest: "sha256:bb".into(),
                    size: 19 * GB / 10,
                    in_use: false,
                },
            ],
            projects: vec![ProjectUsage {
                project: "/Users/me/my-project".into(),
                disk: 31 * GB / 10,
                snapshots: 15 * GB / 10,
                forks: GB / 5,
            }],
            reclaimable: vec![
                Reclaimable {
                    what: "unused images".into(),
                    size: 19 * GB / 10,
                },
                Reclaimable {
                    what: "old automatic snapshots".into(),
                    size: 4 * GB / 10,
                },
            ],
            reclaimed: None,
        };
        assert_eq!(
            table(&usage),
            "Images        2.1 GB   debian (in use), ubuntu (unused, 1.9 GB)\n\
             my-project    4.8 GB   disk 3.1 GB, snapshots 1.5 GB, forks 0.2 GB\n\
             Reclaimable   2.3 GB   unused images 1.9 GB, old automatic snapshots 0.4 GB\n"
        );
    }
}
