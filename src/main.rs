use anyhow::Context;
use notify_rust::{Notification, Urgency};
use scryfall::{
    card::{Card, Game},
    error::ScryfallError,
    set::{SetCode, SetType},
    Error, Set,
};
use std::{
    fmt::Display,
    fs::{self, File},
    io::{self, BufRead, BufReader, BufWriter, Write},
    os::unix::prelude::ExitStatusExt,
    path::Path,
    process::{Command, Stdio},
};
use tempfile::NamedTempFile;

fn notify<T, B>(title: T, body: B)
where
    T: Display,
    B: Display,
{
    Notification::new()
        .summary(&format!("{title}"))
        .body(&format!("{body}"))
        .urgency(Urgency::Low)
        .show()
        .unwrap();
}

fn error(e: anyhow::Error) {
    Notification::new()
        .summary("Error foretelling")
        .body(&format!("{e:?}"))
        .urgency(Urgency::Critical)
        .show()
        .unwrap();
}

fn missing_sets(sets_cache: &Path, cards_cache: &Path) -> anyhow::Result<()> {
    let stored_sets = match File::open(sets_cache) {
        Ok(f) => BufReader::new(f).lines().collect::<Result<Vec<_>, _>>()?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => vec![],
        Err(e) => return Err(e.into()),
    };
    let mut all_sets = Vec::new();
    let mut updated_cards = false;
    let today = chrono::Utc::today().naive_utc();
    for Set {
        code,
        name,
        set_type,
        ..
    } in scryfall::set::Set::all()?
        .filter_map(Result::ok)
        .filter(|s| {
            [
                SetType::Memorabilia,
                SetType::Token,
                SetType::Alchemy,
                SetType::TreasureChest,
                SetType::Promo,
            ]
            .into_iter()
            .all(|t| s.set_type != t)
        })
        .filter(|s| !s.digital)
        .filter(|s| matches!(s.released_at, Some(d) if d <= today))
    {
        all_sets.push(code);
        if stored_sets
            .binary_search_by(|s| s.as_str().cmp(code.get()))
            .is_err()
        {
            println!("updating card list for {name} ({code}) :: {set_type}");
            match update_card_list(cards_cache, code, &name)
                .with_context(|| format!("updating card list for set {name} ({code})"))
            {
                Ok(true) => updated_cards = true,
                Ok(false) => {
                    all_sets.pop();
                }
                Err(e) => {
                    all_sets.pop();
                    error(e);
                }
            }
        }
    }
    if updated_cards {
        all_sets.sort();
        store_sets(sets_cache, &all_sets)?;
    }
    return Ok(());

    fn store_sets(cache: &Path, sets: &[SetCode]) -> anyhow::Result<()> {
        sets.iter()
            .try_fold(BufWriter::new(File::create(&cache)?), |mut file, code| {
                file.write_all(code.get().as_bytes())?;
                file.write_all(b"\n")?;
                io::Result::Ok(file)
            })?;
        Ok(())
    }
}

/// Updates the card list
///
/// if no cards were found for a set, return Ok(false)
/// this happens when new sets are added before their cards are added.
fn update_card_list(path: &Path, set_code: SetCode, set_name: &str) -> anyhow::Result<bool> {
    let mut file = BufWriter::new(
        File::options()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("opening card list at {path:?}"))?,
    );

    const JUST_DONT: &str = "Our Market Research Shows That Players Like Really Long Card Names So We Made this Card to Have the Absolute Longest Card Name Ever Elemental";
    use scryfall::search::prelude::*;
    let today = chrono::Utc::today().naive_utc();
    let count = match Card::search(set(set_code).and(game(Game::Paper)).and(date(lte(today)))) {
        Ok(cards) => cards
            .filter_map(Result::ok)
            .filter(|c| !c.type_line.contains("Basic") && !c.type_line.contains("Token"))
            .map(|c| c.name)
            .filter(|n| n != JUST_DONT)
            .try_fold(0, |count, name| {
                file.write_fmt(format_args!("{name}\n"))?;
                io::Result::Ok(count + 1)
            })?,
        Err(Error::ScryfallError(ScryfallError { status: 404, .. })) => return Ok(false),
        Err(e) => return Err(e.into()),
    };

    notify(
        format!("Set {set_name} ({set_code}) added!"),
        format!("{count} new cards added!"),
    );
    Ok(true)
}

fn card_list() -> anyhow::Result<File> {
    let path = dirs::cache_dir()
        .ok_or_else(|| anyhow::anyhow!("can't find cache dir"))?
        .join("foretell");

    fs::create_dir_all(&path).context("creating cache dir")?;

    println!("getting missing sets");
    let sets_cache = path.join("sets");
    let card_list_file = path.join("cards");
    missing_sets(&sets_cache, &card_list_file).context("getting missing sets")?;

    Ok(File::open(card_list_file)?)
}

fn query() -> anyhow::Result<String> {
    let output = Command::new("dmenu")
        .args(["-p", "scry", "-l", "30", "-i"])
        .stdin(match card_list() {
            Ok(f) => Stdio::from(f),
            Err(e) => {
                error(e);
                Stdio::null()
            }
        })
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
    if query.is_empty() {
        return Ok(());
    }
    let cards = Card::search(&query)?
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
        .try_fold(Vec::new(), |mut acc, v| -> Result<_, Error> {
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
        error(e)
    }
}
