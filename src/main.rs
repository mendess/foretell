use notify_rust::Notification;
use scryfall::card::Card;
use std::{
    os::unix::prelude::ExitStatusExt,
    process::{Command, Stdio},
};
use tempfile::NamedTempFile;

fn query() -> anyhow::Result<String> {
    let output = Command::new("dmenu")
        .args(["-p", "scry"])
        .stdin(Stdio::null())
        .output()?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().into())
    } else if output.status.core_dumped() {
        Err(anyhow::anyhow!("core dumped :("))
    } else if let Some(sig) = output.status.signal() {
        Err(anyhow::anyhow!("killed by signal: {sig}"))
    } else {
        Err(anyhow::anyhow!(
            "process exited with status: {:?}",
            output.status.code()
        ))
    }
}

fn run() -> anyhow::Result<()> {
    let query = dbg!(query()?);
    let cards = Card::search(&query)?
        .inspect(|c| {
            dbg!(c);
        })
        .map(|c| {
            c.map(|mut c| {
                if let Some(faces) = c.card_faces.take() {
                    faces
                        .into_iter()
                        .filter_map(|face| face.image_uris.and_then(|mut u| u.remove("large")))
                        .collect::<Vec<_>>()
                } else {
                    c.image_uris
                        .remove("large")
                        .map(|u| u.to_string())
                        .into_iter()
                        .collect()
                }
            })
        })
        .try_fold(Vec::new(), |mut acc, v| -> Result<_, scryfall::Error> {
            acc.extend(v?);
            Ok(acc)
        })?;

    let client = reqwest::blocking::Client::new();
    let files = cards
        .into_iter()
        .map(|card| -> anyhow::Result<NamedTempFile> {
            let mut file = NamedTempFile::new()?;
            client.get(card).send()?.copy_to(&mut file)?;
            Ok(file)
        })
        .collect::<Result<Vec<_>, _>>()?;

    Command::new("sxiv")
        .args(["-b", "-g", "590x800"])
        .args(files.iter().map(|f| f.path()))
        .spawn()?
        .wait()?;
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        Notification::new()
            .summary("Error foretelling")
            .body(&format!("{e:?}"))
            .show()
            .unwrap();
    }
}
