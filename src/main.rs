use anyhow::Context;
use futures_util::{stream::StreamExt, TryStreamExt};
use notify_rust::{Notification, NotificationHandle, Urgency};
use scryfall::{
    card::{Card, Game},
    error::ScryfallError,
    set::{SetCode, SetType},
    Error, Set,
};
use std::{
    collections::HashSet, fmt::Display, future, os::unix::process::ExitStatusExt, path::Path,
    process::Stdio,
};
use tempfile::NamedTempFile;
use tokio::{
    fs::{self, File, OpenOptions},
    io::{self, AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter},
    process::Command,
    sync::Mutex,
    task::{self, JoinHandle},
};
use tokio_stream::wrappers::LinesStream;

fn notify<T, B>(title: T, body: B) -> Option<NotificationHandle>
where
    T: Display,
    B: Display,
{
    let summary = format!("{title}");
    let body = format!("{body}");
    let e = Notification::new()
        .summary(&summary)
        .body(&body)
        .urgency(Urgency::Low)
        .show();
    match e {
        Ok(h) => Some(h),
        Err(e) => {
            println!("failed to notify: {e}");
            backup_notify(&summary, &body, "low");
            None
        }
    }
}

fn error(e: anyhow::Error) {
    let summary = "Error foretelling";
    let body = format!("{e:?}");
    let e = Notification::new()
        .summary(summary)
        .body(&body)
        .urgency(Urgency::Critical)
        .show();
    if let Err(e) = e {
        println!("failed to notify error: {e}");
        backup_notify(summary, &body, "critical");
    }
}

fn backup_notify(summary: &str, body: &str, urgency: &str) {
    let child = Command::new("notify-send")
        .args([summary, body, "-u", urgency])
        .spawn();
    if let Ok(mut child) = child {
        task::spawn(async move {
            let _ = child.wait().await;
        });
    }
}

async fn missing_sets(sets_cache: &Path, cards_cache: &Path) -> anyhow::Result<()> {
    let stored_sets = match File::open(sets_cache).await {
        Ok(f) => {
            LinesStream::new(BufReader::new(f).lines())
                .try_collect::<Vec<_>>()
                .await?
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => vec![],
        Err(e) => return Err(e.into()),
    };
    let mut all_sets = Vec::new();
    let mut updated_cards = false;
    let today = chrono::Utc::now().naive_utc().date();
    let sets = scryfall::set::Set::all()
        .await?
        .into_stream()
        .filter_map(|o| future::ready(o.ok()))
        .filter(|s| {
            future::ready(
                [
                    SetType::Memorabilia,
                    SetType::Token,
                    SetType::Alchemy,
                    SetType::TreasureChest,
                    SetType::Promo,
                ]
                .into_iter()
                .all(|t| s.set_type != t),
            )
        })
        .filter(|s| future::ready(!s.digital))
        .filter(|s| future::ready(matches!(s.released_at, Some(d) if d <= today)));
    tokio::pin!(sets);
    while let Some(Set {
        code,
        name,
        set_type,
        ..
    }) = sets.next().await
    {
        all_sets.push(code);
        if stored_sets
            .binary_search_by(|s| s.as_str().cmp(code.get()))
            .is_err()
        {
            println!("updating card list for {name} ({code}) :: {set_type}");
            match update_card_list(cards_cache, code, &name)
                .await
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
        store_sets(sets_cache, &all_sets).await?;
    }
    return Ok(());

    async fn store_sets(cache: &Path, sets: &[SetCode]) -> anyhow::Result<()> {
        let mut file = BufWriter::new(File::create(&cache).await?);
        for code in sets {
            file.write_all(code.get().as_bytes()).await?;
            file.write_all(b"\n").await?;
        }
        Ok(())
    }
}

/// Updates the card list
///
/// if no cards were found for a set, return Ok(false)
/// this happens when new sets are added before their cards are added.
async fn update_card_list(path: &Path, set_code: SetCode, set_name: &str) -> anyhow::Result<bool> {
    let mut file = BufWriter::new(
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await
            .with_context(|| format!("opening card list at {path:?}"))?,
    );

    const JUST_DONT: &str = "Our Market Research Shows That Players Like Really Long Card Names So We Made this Card to Have the Absolute Longest Card Name Ever Elemental";
    use scryfall::search::prelude::*;
    let today = chrono::Utc::now().naive_utc().date();
    let count = match Card::search(set(set_code).and(game(Game::Paper)).and(date(lte(today)))).await
    {
        Ok(cards) => {
            let file = &mut file;
            let card_names = cards
                .into_stream()
                .filter_map(|o| future::ready(o.ok()))
                .filter(|c| -> future::Ready<bool> {
                    future::ready(
                        c.type_line.as_deref() != Some("Basic")
                            && c.type_line.as_deref() != Some("Token"),
                    )
                })
                .map(|c| c.name)
                .filter(|n| future::ready(n != JUST_DONT));
            tokio::pin!(card_names);
            let mut count = 0;
            while let Some(name) = card_names.next().await {
                file.write_all(name.as_bytes()).await?;
                file.write_all(b"\n").await?;
                count += 1;
            }
            count
        }
        Err(Error::ScryfallError(ScryfallError { status: 404, .. })) => return Ok(false),
        Err(e) => return Err(e.into()),
    };

    notify(
        format!("Set {set_name} ({set_code}) added!"),
        format!("{count} new cards added!"),
    );
    Ok(true)
}

static BACKGROUND_UPDATE: Mutex<Option<JoinHandle<()>>> = Mutex::const_new(None);

async fn card_list() -> anyhow::Result<File> {
    let path = dirs::cache_dir()
        .ok_or_else(|| anyhow::anyhow!("can't find cache dir"))?
        .join("foretell");

    fs::create_dir_all(&path)
        .await
        .context("creating cache dir")?;

    println!("getting missing sets");
    let sets_cache = path.join("sets");
    let card_list_file = path.join("cards");
    let lock_file = path.join("lock");
    let _ = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&lock_file);
    match fmutex::try_lock(&lock_file) {
        Ok(Some(guard)) => {
            let handle = tokio::spawn({
                let card_list_file = card_list_file.clone();
                async move {
                    if let Err(e) = missing_sets(&sets_cache, &card_list_file).await {
                        error(e)
                    }
                    drop(guard);
                }
            });
            *BACKGROUND_UPDATE.lock().await = Some(handle);
        }
        Ok(None) => {}
        Err(e) => error(anyhow::anyhow!("failed to lock {lock_file:?}: {e}")),
    }

    match File::open(card_list_file).await {
        Ok(f) => Ok(f),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            File::open("/dev/null").await.map_err(Into::into)
        }
        Err(e) => Err(e.into()),
    }
}

async fn query() -> anyhow::Result<String> {
    let card_list_file = match card_list().await {
        Ok(f) => f,
        Err(e) => {
            error(e);
            File::open("/dev/null").await?
        }
    };
    let mut dmenu = Command::new("dmenu")
        .args(["-p", "scry", "-l", "30", "-i"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;
    {
        let mut line = String::new();
        let mut reader = BufReader::new(card_list_file);
        let mut pipe = BufWriter::new(dmenu.stdin.take().expect("stdin was piped"));
        let mut seen = HashSet::new();
        while reader.read_line(&mut line).await? > 0 {
            if !seen.contains(&line) {
                seen.insert(line.clone());
                pipe.write_all(line.as_bytes()).await?;
            }
            line.clear();
        }
    }
    let output = dmenu.wait_with_output().await?;
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

struct ProgressNotifier {
    total: usize,
    current: usize,
    last_notif: usize,
    notification_handle: Option<NotificationHandle>,
}

impl ProgressNotifier {
    fn new(total: usize) -> Self {
        let mut this = Self {
            total,
            current: 0,
            last_notif: 0,
            notification_handle: None,
        };
        this.notify();
        this
    }

    fn progress(&mut self) {
        self.current += 1;
        self.notify();
    }

    fn notify(&mut self) {
        if (self.current * 10 / self.total) == self.last_notif {
            let body = format!("{}/{} done", self.current, self.total);
            match self.notification_handle.as_mut() {
                Some(handle) => {
                    handle.body(&body);
                    handle.update();
                }
                None => {
                    self.notification_handle = notify("Downloading", body);
                }
            }
            self.last_notif += 1;
        }
    }
}

async fn run() -> anyhow::Result<()> {
    let query = query().await?;
    if query.is_empty() {
        return Ok(());
    }
    let cards = Card::search(&query)
        .await?
        .into_stream()
        .map(|c| {
            c.map(|mut c| {
                let uris = if let Some(large) = c.image_uris.remove("large") {
                    vec![large.to_string()]
                } else if let Some(faces) = c.card_faces.take() {
                    faces
                        .into_iter()
                        .filter_map(|face| face.image_uris.and_then(|mut u| u.remove("large")))
                        .collect::<Vec<_>>()
                } else {
                    vec![]
                };
                if uris.is_empty() {
                    error(anyhow::anyhow!(
                        "failed to get any uris for card {}",
                        c.name
                    ));
                }
                uris
            })
        })
        .try_fold(Vec::new(), |mut acc, v| async move {
            acc.extend(v);
            Ok::<_, Error>(acc)
        })
        .await?;

    if cards.is_empty() {
        return Err(anyhow::anyhow!("no cards found"));
    }

    let client = reqwest::blocking::Client::new();
    let mut progress = ProgressNotifier::new(cards.len());
    let files = cards
        .into_iter()
        .map(|card| -> anyhow::Result<NamedTempFile> {
            let mut file = NamedTempFile::new()?;
            client.get(card).send()?.copy_to(&mut file)?;
            progress.progress();
            Ok(file)
        })
        .collect::<Result<Vec<_>, _>>()?;

    let status = Command::new("sxiv")
        .args(["-b", "-g", "590x800"])
        .args(files.iter().map(|f| f.path()))
        .spawn()?
        .wait()
        .await?;
    if !status.success() {
        Err(anyhow::anyhow!("sxiv error {status}"))
    } else {
        Ok(())
    }
}

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        error(e)
    }
    let mut j = BACKGROUND_UPDATE.lock().await;
    if let Some(j) = j.take() {
        if let Err(e) = j.await {
            error(anyhow::anyhow!("background update thread panicked: {e:?}"));
        } else {
            println!("background task ended");
        }
    }
}
