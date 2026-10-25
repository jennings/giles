//! WhatsApp spike: link `whatsapp-rust` as a companion device (QR code or pair
//! code), print incoming messages, send a DM, and reuse the persisted session after a restart.
//! Test plan and findings: `Docs/technical/whatsapp-spike.md`.

mod ids;

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use qrcode::QrCode;
use qrcode::render::unicode::Dense1x2;
use tokio::time::{Instant, sleep_until, timeout};
use whatsapp_rust::async_channel::Receiver;
use whatsapp_rust::pair_code::PairCodeOptions;
use whatsapp_rust::prelude::*;
use whatsapp_rust::types::events::ChannelEventHandler;

use crate::ids::{ChatKind, Echo, SenderIds, SentLog, parse_phone, sender_ids};

/// How long `link` stays connected after pairing, so the first connection can finish its setup
/// (pre-key upload, initial app-state sync) before the process exits.
const LINK_SETTLE: Duration = Duration::from_secs(20);
/// How long `send` waits to connect, and then for the server's ack of the message.
const SEND_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Parser)]
#[command(about = "WhatsApp spike: link, listen, and send with whatsapp-rust")]
struct Cli {
    /// Directory for the session store and spike state (created with mode 0700). Only one
    /// process may use it at a time.
    #[arg(long, value_name = "DIR")]
    data: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Link this spike as a companion device of a WhatsApp account.
    Link(LinkArgs),
    /// Print incoming messages (DMs and groups) until Ctrl-C.
    Listen,
    /// Send a text DM to a phone number, wait for the server's ack, and exit.
    Send {
        /// Recipient phone number with country code, e.g. +15551234567.
        phone: String,
        /// Message text.
        text: String,
    },
}

#[derive(Args)]
struct LinkArgs {
    /// Show QR codes to scan in WhatsApp > Linked devices > Link a device.
    #[arg(
        long,
        required_unless_present = "pair_code",
        conflicts_with = "pair_code"
    )]
    qr: bool,
    /// Draw the QR code for a terminal with a light background (the default suits dark ones).
    #[arg(long, requires = "qr")]
    light_terminal: bool,
    /// Request an 8-character pair code for this phone number (the account's own number, with
    /// country code), to enter on the phone under "Link with phone number instead".
    #[arg(long, value_name = "PHONE")]
    pair_code: Option<String>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    // Validate phone numbers before touching the network.
    let pair_phone = match &cli.command {
        Command::Link(LinkArgs {
            pair_code: Some(phone),
            ..
        }) => Some(parse_phone(phone)?),
        _ => None,
    };
    let send_to = match &cli.command {
        Command::Send { phone, .. } => Some(parse_phone(phone)?),
        _ => None,
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(run(cli, pair_phone, send_to))
}

async fn run(cli: Cli, pair_phone: Option<String>, send_to: Option<String>) -> Result<()> {
    let data = DataDir::open(&cli.data)?;
    let db = data.root.join("whatsapp.db");
    let db_url = db.to_str().context("data directory path is not UTF-8")?;
    let store = SqliteStore::new(db_url)
        .await
        .with_context(|| format!("opening session store {}", db.display()))?;

    // Giles never imports chat history, so neither does the spike.
    let mut builder = Bot::builder().with_backend(store).skip_history_sync();
    if let Some(phone) = pair_phone.clone() {
        builder = builder.with_pair_code(PairCodeOptions {
            phone_number: phone,
            ..Default::default()
        });
    }
    let bot = builder
        .build()
        .await
        .context("building the WhatsApp client")?;
    let client = bot.client();

    let device = client.persistence_manager().get_device_snapshot();
    let linked = device.pn.clone();
    match (&cli.command, &linked) {
        (Command::Link(_), Some(pn)) => {
            println!(
                "already linked as {} (lid {}); session store {}",
                pn,
                display_opt(device.lid.as_ref()),
                db.display()
            );
            println!(
                "to relink, unlink the device on the phone and delete {}",
                data.root.display()
            );
            return Ok(());
        }
        (Command::Listen | Command::Send { .. }, None) => {
            bail!("not linked: run `link --qr` or `link --pair-code <phone>` first");
        }
        _ => {}
    }
    if let Some(pn) = &linked {
        println!(
            "reusing session for {} (lid {}) from {}",
            pn,
            display_opt(device.lid.as_ref()),
            db.display()
        );
    }

    // Raw handlers run inline on the event bus, so this channel sees every event in order.
    let (handler, events) = ChannelEventHandler::new();
    let _subscription = client.subscribe_handler(handler);
    let handle = bot.spawn();

    let result = tokio::select! {
        result = async {
            match cli.command {
                Command::Link(args) => link(&client, &events, &args).await,
                Command::Listen => listen(&client, &events, &data).await,
                Command::Send { text, .. } => {
                    let to = send_to.expect("validated in main");
                    send(&client, &events, &data, &to, &text).await
                }
            }
        } => result,
        _ = tokio::signal::ctrl_c() => {
            println!("interrupted");
            Ok(())
        }
    };
    // Disconnect gracefully so the device state, receipts, and Signal sessions are flushed.
    if timeout(Duration::from_secs(10), handle.shutdown())
        .await
        .is_err()
    {
        eprintln!("warning: shutdown took longer than 10 s; exiting anyway");
    }
    result
}

async fn link(client: &Arc<Client>, events: &Receiver<Arc<Event>>, args: &LinkArgs) -> Result<()> {
    let mut paired = false;
    let mut settle_until: Option<Instant> = None;
    loop {
        let event = match settle_until {
            Some(deadline) => tokio::select! {
                event = events.recv() => event,
                () = sleep_until(deadline) => {
                    println!("linked; the session is saved, so `listen` and `send` won't relink");
                    return Ok(());
                }
            },
            None => events.recv().await,
        }
        .context("client stopped")?;
        match &*event {
            Event::PairingQrCode(qr) if args.qr => {
                println!("{}", render_qr(&qr.code, args.light_terminal)?);
                println!(
                    "scan in WhatsApp > Linked devices > Link a device (valid {} s)",
                    qr.timeout.as_secs()
                );
            }
            Event::PairingQrCodesExhausted(e) if e.disconnected => {
                bail!("no QR code was scanned in time; run `link` again");
            }
            Event::PairingCode(code) => {
                println!("pair code: {}", code.code);
                println!(
                    "enter it in WhatsApp > Linked devices > Link a device > Link with phone number \
                     instead (valid {} s)",
                    code.timeout.as_secs()
                );
            }
            Event::PairingCodeError(e) => bail!("pair code request failed: {}", e.error),
            Event::PairingCodeRefresh(_) => {
                bail!("the pair code was invalidated; run `link --pair-code` again for a new one");
            }
            Event::QrScannedWithoutMultidevice(_) => {
                println!("the phone scanned the code but has multi-device disabled");
            }
            Event::PairSuccess(info) => {
                paired = true;
                println!(
                    "paired: phone {} lid {} platform {:?} business name {:?}",
                    info.id, info.lid, info.platform, info.business_name
                );
            }
            Event::PairError(e) => bail!("pairing failed: {}", e.error),
            Event::Connected(_) if paired || client.is_logged_in() => {
                println!(
                    "connected; staying online {} s to finish setup (Ctrl-C to stop early)",
                    LINK_SETTLE.as_secs()
                );
                settle_until.get_or_insert_with(|| Instant::now() + LINK_SETTLE);
            }
            Event::Disconnected(d) => {
                println!("disconnected ({:?}); the client reconnects", d.reason)
            }
            Event::LoggedOut(l) => bail!("logged out: {:?}", l.reason),
            Event::ClientOutdated(_) => bail!("WhatsApp rejected this client version as outdated"),
            _ => {}
        }
    }
}

async fn listen(client: &Arc<Client>, events: &Receiver<Arc<Event>>, data: &DataDir) -> Result<()> {
    println!("listening; Ctrl-C to stop");
    loop {
        let event = events.recv().await.context("client stopped")?;
        match &*event {
            Event::Connected(_) => println!("connected"),
            Event::Disconnected(d) => {
                println!("disconnected ({:?}); the client reconnects", d.reason)
            }
            Event::Messages(batch) => {
                // Reread the log so sends from a `send` run in between are recognized.
                let sent = data.sent_log()?;
                for message in batch.messages.iter() {
                    println!("{}", describe(client, message, &sent).await);
                }
            }
            Event::UndecryptableMessage(u) => println!(
                "undecryptable: id={} chat={} sender={} offline={}",
                u.info.id, u.info.source.chat, u.info.source.sender, u.info.is_offline
            ),
            Event::PairingQrCode(_) | Event::PairingCode(_) => {
                bail!("the server asked to pair again: the saved session is no longer valid");
            }
            Event::LoggedOut(l) => bail!("logged out: {:?}", l.reason),
            Event::ClientOutdated(_) => bail!("WhatsApp rejected this client version as outdated"),
            _ => {}
        }
    }
}

/// One line per received message: everything the Giles plugin would map into `InboundMessage`.
async fn describe(client: &Client, message: &InboundMessage, sent: &SentLog) -> String {
    let info = &message.info;
    let source = &info.source;
    let mapping = if source.sender_alt.is_none() {
        client.get_lid_pn_entry(&source.sender).await.ok().flatten()
    } else {
        None
    };
    let SenderIds { phone, lid } = sender_ids(
        &source.sender,
        source.sender_alt.as_ref(),
        mapping.as_ref().map(|e| (&*e.lid, &*e.phone_number)),
    );
    let body = message
        .message
        .text_content()
        .or_else(|| message.message.get_caption());
    format!(
        "message id={} at={} chat={} kind={} sender={} sender_phone={} sender_lid={} \
         push_name={:?} echo={} offline={} type={} media={:?} text={:?}",
        info.id,
        info.timestamp.to_rfc3339(),
        source.chat,
        ChatKind::of(&source.chat, source.is_group),
        source.sender,
        phone.as_deref().unwrap_or("-"),
        lid.as_deref().unwrap_or("-"),
        info.push_name,
        Echo::classify(source.is_from_me, &info.id, sent),
        info.is_offline,
        info.r#type,
        info.media_type,
        body.unwrap_or(""),
    )
}

async fn send(
    client: &Arc<Client>,
    events: &Receiver<Arc<Event>>,
    data: &DataDir,
    to: &str,
    text: &str,
) -> Result<()> {
    let deadline = Instant::now() + SEND_TIMEOUT;
    if !client.is_logged_in() {
        wait_for(events, deadline, "connecting", |event| match event {
            Event::Connected(_) => Ok(true),
            Event::LoggedOut(l) => bail!("logged out: {:?}", l.reason),
            Event::PairingQrCode(_) => bail!("the saved session is no longer valid; relink"),
            _ => Ok(false),
        })
        .await?;
    }
    let result = client
        .send_text(Jid::pn(to), text)
        .await
        .context("sending the message")?;
    data.record_sent(&result.message_id)?;
    println!("sent id={} to={}", result.message_id, result.to);

    let deadline = Instant::now() + SEND_TIMEOUT;
    wait_for(events, deadline, "the server's ack", |event| match event {
        Event::ServerAck(ack) if ack.id == result.message_id => match &ack.error {
            Some(code) => bail!("the server rejected the message (code {code})"),
            None => {
                println!("server ack at {}", display_opt(ack.timestamp.as_ref()));
                Ok(true)
            }
        },
        _ => Ok(false),
    })
    .await
}

/// Feeds events to `done` until it returns `true`, an error, or the deadline passes.
async fn wait_for(
    events: &Receiver<Arc<Event>>,
    deadline: Instant,
    what: &str,
    mut done: impl FnMut(&Event) -> Result<bool>,
) -> Result<()> {
    loop {
        let event = tokio::select! {
            event = events.recv() => event.context("client stopped")?,
            () = sleep_until(deadline) => bail!("timed out waiting for {what}"),
        };
        if done(&event)? {
            return Ok(());
        }
    }
}

/// Renders a QR payload as Unicode half blocks for a terminal, with the standard quiet zone.
///
/// `Dense1x2::Dark` is a filled block, which a terminal draws in its foreground color. On a dark
/// terminal the foreground is light, so dark modules are drawn as blanks and light ones (and the
/// quiet zone) as blocks; `light_terminal` uses the blocks for the dark modules instead.
fn render_qr(payload: &str, light_terminal: bool) -> Result<String> {
    let code = QrCode::new(payload.as_bytes()).context("encoding the QR code")?;
    let (dark, light) = if light_terminal {
        (Dense1x2::Dark, Dense1x2::Light)
    } else {
        (Dense1x2::Light, Dense1x2::Dark)
    };
    Ok(code
        .render::<Dense1x2>()
        .dark_color(dark)
        .light_color(light)
        .quiet_zone(true)
        .build())
}

fn display_opt(value: Option<&impl std::fmt::Display>) -> String {
    value.map_or_else(|| "-".to_owned(), ToString::to_string)
}

/// The `--data` directory, locked for this process.
struct DataDir {
    root: PathBuf,
    /// Holds an exclusive `flock` on `<data>/lock` until the process exits: two clients on one
    /// session would knock each other off the server.
    _lock: File,
}

impl DataDir {
    fn open(root: &Path) -> Result<Self> {
        fs::create_dir_all(root).with_context(|| format!("creating {}", root.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(root, fs::Permissions::from_mode(0o700))?;
        }
        let lock_path = root.join("lock");
        let lock = File::create(&lock_path)?;
        if lock.try_lock().is_err() {
            bail!("{} is in use by another spike process", root.display());
        }
        Ok(Self {
            root: root.to_owned(),
            _lock: lock,
        })
    }

    fn sent_path(&self) -> PathBuf {
        self.root.join("sent-ids")
    }

    fn sent_log(&self) -> Result<SentLog> {
        match fs::read_to_string(self.sent_path()) {
            Ok(contents) => Ok(SentLog::parse(&contents)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(SentLog::default()),
            Err(e) => Err(e).context("reading the sent log"),
        }
    }

    fn record_sent(&self, id: &str) -> Result<()> {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.sent_path())?;
        file.write_all(SentLog::line(id).as_bytes())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qr_renders_a_square_with_a_quiet_zone_in_either_polarity() {
        // A realistic five-field pairing payload: ref, three base64 keys, client type.
        let key = "A".repeat(43) + "=";
        let payload = format!("2@{},{key},{key},{key},1", "x".repeat(80));
        let dark = render_qr(&payload, false).unwrap();
        let light = render_qr(&payload, true).unwrap();
        // QR codes are an odd number of modules square (plus 4 quiet modules per side), so the
        // last character row holds one module row: the light quiet zone in its upper half.
        for (art, quiet, last) in [(&dark, '█', '▀'), (&light, ' ', ' ')] {
            let lines: Vec<&str> = art.lines().collect();
            let width = lines[0].chars().count();
            assert!(lines.iter().all(|l| l.chars().count() == width));
            assert!(width >= 21 + 8);
            assert_eq!(lines.len(), width.div_ceil(2));
            let (bottom, rest) = lines.split_last().unwrap();
            assert!(rest[0].chars().all(|c| c == quiet), "top: {:?}", rest[0]);
            assert!(
                rest.iter()
                    .all(|l| l.starts_with(quiet) && l.ends_with(quiet))
            );
            assert!(bottom.chars().all(|c| c == last), "bottom: {bottom:?}");
        }
        // Apart from that half row, the two polarities are exact inverses.
        let invert = |c| match c {
            '█' => ' ',
            ' ' => '█',
            '▀' => '▄',
            '▄' => '▀',
            other => other,
        };
        let body = |art: &str| art.lines().count() - 1;
        let inverted: Vec<String> = dark
            .lines()
            .map(|l| l.chars().map(invert).collect())
            .collect();
        let light_lines: Vec<&str> = light.lines().collect();
        assert_eq!(inverted[..body(&dark)], light_lines[..body(&light)]);
    }

    #[test]
    fn data_dir_is_private_and_exclusive() {
        let dir = std::env::temp_dir().join(format!("whatsapp-spike-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let data = DataDir::open(&dir).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        assert!(
            DataDir::open(&dir).is_err(),
            "a second open must fail while locked"
        );

        assert!(!data.sent_log().unwrap().contains("3EB0AAAA"));
        data.record_sent("3EB0AAAA").unwrap();
        data.record_sent("3EB0BBBB").unwrap();
        let sent = data.sent_log().unwrap();
        assert!(sent.contains("3EB0AAAA") && sent.contains("3EB0BBBB"));

        drop(data);
        assert!(
            DataDir::open(&dir).is_ok(),
            "the lock is released with the process's handle"
        );
        fs::remove_dir_all(&dir).unwrap();
    }
}
