use std::{
    fmt,
    io::{self, BufRead, BufReader, Read, Write},
    process::{exit, Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::Duration,
};

use anyhow::{anyhow, bail, Context, Result};
use dialoguer::Select;
use serde_json::Value;

fn slug(s: &str) -> String {
    s.to_ascii_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect()
}

fn wait_cmd(child: &mut Child, done: &Arc<AtomicBool>) -> Result<()> {
    loop {
        match child.try_wait() {
            Ok(None) => (),
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => bail!("failed: {status}"),
            Err(e) => bail!("error: {e}"),
        };

        if done.load(Ordering::SeqCst) {
            child.kill()?;
            bail!("Ctrl+C");
        }

        thread::sleep(Duration::from_millis(100));
    }
}

#[derive(Debug)]
struct Run {
    run_id: String,
    vod_uri: String,
    player: String,
    game: String,
    game_name: String,
    cat_full: String,
    cat: String,
    time: String,
}

impl Run {
    fn filename(&self) -> String {
        format!("{}-{}-{}-{}", self.player, self.game, self.cat, self.run_id)
    }
}

impl TryFrom<&Value> for Run {
    type Error = anyhow::Error;

    fn try_from(value: &Value) -> Result<Self> {
        let run_id = value["id"]
            .as_str()
            .context("Can't read run ID")?
            .to_string();
        let vod_uri = value["videos"]["links"][0]["uri"]
            .as_str()
            .context("Can't read VOD URI")?
            .to_string();
        let player = value["players"]["data"][0]["names"]["international"]
            .as_str()
            .context("Can't read player data")?
            .to_string();
        let game = value["game"]["data"]["abbreviation"]
            .as_str()
            .context("Can't read game data")?
            .to_string();
        let game_name = value["game"]["data"]["names"]["twitch"]
            .as_str()
            .context("Can't read game name")?
            .to_string();
        let cat_full = value["category"]["data"]["name"]
            .as_str()
            .context("Can't read category name")?
            .to_string();
        let cat = slug(&cat_full);
        let time = {
            let d = iso8601_duration::Duration::parse(
                value["times"]["primary"]
                    .as_str()
                    .context("Couldn't read run time")?,
            )
            .map_err(|e| anyhow!("{e:?}"))
            .context("Couldn't parse run time")?
            .to_std()
            .unwrap();

            let as_secs = d.as_secs();
            let s = as_secs % 60;
            let m = (as_secs / 60) % 60;
            let h = as_secs / 3600;

            format!("{h:02}:{m:02}:{s:02}")
        };

        Ok(Self {
            run_id,
            vod_uri,
            player,
            game,
            game_name,
            cat_full,
            cat,
            time,
        })
    }
}

impl fmt::Display for Run {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "\x1b[33m{} \x1b[0m- \x1b[34m{}\x1b[0m in \x1b[32m{}\x1b[0m by \x1b[32m{}\x1b[0m",
            self.game_name, self.cat_full, self.time, self.player,
        )
    }
}

async fn get_pending_runs(game: &str) -> Result<Vec<Run>> {
    let api_uri = format!(
        "https://www.speedrun.com/api/v1/runs?game={game}&status=new&embed=players,game,category&max=100"
    );

    let body = reqwest::get(api_uri)
        .await
        .context("Requesting runs metadata")?
        .text()
        .await
        .context("Reading run metadata")?;

    let runs: Value = serde_json::from_str(&body).context("Parsing run metadata")?;
    runs["data"]
        .as_array()
        .context("Unexpected value")?
        .iter()
        .map(Run::try_from)
        .collect()
}

async fn download_run(run: &Run, done: &Arc<AtomicBool>) -> Result<()> {
    let filename = run.filename();

    println!("\nDownloading {run}");

    let mut yt_dlp_cmd = Command::new("yt-dlp");
    yt_dlp_cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .args([
            &run.vod_uri,
            "-N",
            "8",
            "--progress",
            "--newline",
            "-q",
            "-o",
            "-",
        ]);

    #[cfg(not(target_os = "macos"))]
    let ffmpeg_args = [
        "-y",
        "-i",
        "pipe:",
        "-c:v",
        "h264_nvenc",
        "-x264-params",
        "keyint=30:min-keyint=30:no-scenecut=1",
        "-filter:v",
        "fps=30, scale=896:-1",
        "-c:a",
        "aac",
        "-b:a",
        "96k",
        "-ar",
        "44100",
    ];
    #[cfg(target_os = "macos")]
    let ffmpeg_args = [
        "-y",
        "-i",
        "pipe:",
        "-c:v",
        "h264_videotoolbox",
        "-x264-params",
        "keyint=30:min-keyint=30:no-scenecut=1",
        "-filter:v",
        "fps=30, scale=896:-1",
        "-c:a",
        "aac",
        "-b:a",
        "96k",
        "-ar",
        "44100",
        "-prio_speed",
        "true",
    ];

    let mut ffmpeg_cmd = Command::new("ffmpeg");
    ffmpeg_cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .args(ffmpeg_args)
        .arg(format!("{filename}.mp4"));

    let mut yt_dlp_child = yt_dlp_cmd.spawn()?;
    let mut ffmpeg_child = ffmpeg_cmd.spawn()?;

    let mut yt_dlp_stdout = yt_dlp_child.stdout.take().unwrap();
    let yt_dlp_stderr = yt_dlp_child.stderr.take().unwrap();
    let mut ffmpeg_stdin = ffmpeg_child.stdin.take().unwrap();

    let stderr_thread = thread::spawn(move || {
        let mut buf = String::new();
        let mut reader = BufReader::new(yt_dlp_stderr);

        loop {
            buf.clear();
            match reader.read_line(&mut buf) {
                Ok(c) if c > 0 => c,
                _ => break,
            };

            print!("\r\x1b[2K\r{}", buf.trim_end());
            io::stdout().flush().unwrap();
        }
    });

    let mut buf = [0u8; 4096];

    loop {
        let bytes_read = yt_dlp_stdout
            .read(&mut buf)
            .context("Couldn't read from yt-dlp")?;

        ffmpeg_stdin
            .write(&buf[0..bytes_read])
            .context("Couldn't write to ffmpeg")?;

        if bytes_read == 0 || done.load(Ordering::SeqCst) {
            drop(yt_dlp_stdout);
            drop(ffmpeg_stdin);
            break;
        }
    }
    println!("\nDone!");

    wait_cmd(&mut yt_dlp_child, done).context("yt-dlp process")?;
    wait_cmd(&mut ffmpeg_child, done).context("ffmpeg process")?;
    stderr_thread
        .join()
        .map_err(|e| anyhow!("I/O error: {e:?}"))?;

    Ok(())
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

    let mut runs = Vec::new();
    runs.extend(get_pending_runs("nd28z0ed").await?);
    runs.extend(get_pending_runs("k6qg0xdg").await?);

    let choices = runs.iter().map(|run| run.to_string()).collect::<Vec<_>>();
    let choice = Select::new()
        .with_prompt("Choose a run")
        .default(0)
        .items(&choices[..])
        .interact_opt()?;

    if let Some(choice) = choice {
        download_run(&runs[choice], &done).await?;
    }

    Ok(())
}
