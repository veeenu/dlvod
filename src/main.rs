use std::{
    env,
    path::PathBuf,
    process::{exit, Command},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::Duration,
};

use anyhow::{anyhow, bail, Result};
use serde_json::Value;

fn slug(s: &str) -> String {
    s.to_ascii_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect()
}

fn run_cmd(cmd: &mut Command, done: Arc<AtomicBool>) -> Result<()> {
    let mut cmd = cmd.spawn()?;

    loop {
        match cmd.try_wait() {
            Ok(None) => (),
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => bail!("failed: {status}"),
            Err(e) => bail!("error: {e}"),
        };

        if done.load(Ordering::SeqCst) {
            cmd.kill()?;
            bail!("Ctrl+C");
        }

        thread::sleep(Duration::from_millis(100));
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let done = Arc::new(AtomicBool::new(false));

    ctrlc::set_handler({
        let done = Arc::clone(&done);
        move || {
            done.store(true, Ordering::SeqCst);
            thread::sleep(Duration::from_millis(1000));
            exit(1);
        }
    })?;

    let uri = env::args().nth(1).unwrap();
    let run_id = uri.rsplit_once('/').unwrap().1;

    let api_uri =
        format!("https://www.speedrun.com/api/v1/runs/{run_id}?embed=players,game,category");

    let body = reqwest::get(api_uri).await.unwrap().text().await.unwrap();

    let run: Value = serde_json::from_str(&body).unwrap();

    let vod_uri = run["data"]["videos"]["links"][0]["uri"]
        .as_str()
        .expect("URI")
        .to_string();
    let player = run["data"]["players"]["data"][0]["names"]["international"]
        .as_str()
        .unwrap();
    let game = run["data"]["game"]["data"]["abbreviation"]
        .as_str()
        .unwrap();
    let game_name = run["data"]["game"]["data"]["names"]["twitch"]
        .as_str()
        .unwrap();
    let cat_full = run["data"]["category"]["data"]["name"].as_str().unwrap();
    let cat = slug(cat_full);

    let filename = format!("{player}-{game}-{cat}-{run_id}");
    let dl_filename = format!("dl-{filename}");

    // yt-dlp "https://www.youtube.com/watch?v=Ee0W5xsa_lE" -N 8 -o vod --progress --newline
    println!(
        "\nDownloading \x1b[32m{player}\x1b[0m - \x1b[33m{game_name} \x1b[0m- \x1b[34m{cat_full}\x1b[0m\n"
    );

    if let Err(e) = thread::spawn({
        let dl_filename = dl_filename.clone();
        let done = Arc::clone(&done);
        move || {
            run_cmd(
                Command::new("yt-dlp").args([
                    &vod_uri,
                    "-f",
                    "b",
                    "-S",
                    "filesize:1G",
                    "-N",
                    "8",
                    "--progress",
                    "-q",
                    "-o",
                    &dl_filename,
                ]),
                done,
            )
        }
    })
    .join()
    {
        bail!("{e:?}");
    }

    let dl_path = PathBuf::from(&dl_filename);

    let dl_path = if dl_path.exists() {
        Some(dl_path)
    } else {
        ["mkv", "mp4", "webm"]
            .iter()
            .map(|x| dl_path.clone().with_extension(x))
            .find(|p| p.exists())
    }
    .ok_or_else(|| anyhow!("{dl_filename}: Output file not found"))?;

    if let Err(e) = thread::spawn({
        let done = Arc::clone(&done);
        move || {
            run_cmd(
                Command::new("ffmpeg")
                    .arg("-i")
                    .arg(dl_path)
                    .args([
                        "-c:v",
                        "hevc_videotoolbox",
                        "-filter:v",
                        "fps=30, scale=1280:-1",
                        "-c:a",
                        "copy",
                        "-prio_speed",
                        "true",
                    ])
                    .arg(format!("{filename}.mp4")),
                done,
            )
        }
    })
    .join()
    {
        bail!("{e:?}");
    }

    Ok(())
}
