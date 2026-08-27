//! Human-facing Orbit CLI.

use std::{
    fs::OpenOptions,
    path::PathBuf,
    process::Stdio,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use orbit_core::{
    config::{Config, OrbitPaths},
    install, ipc,
    model::{Request, Response},
    store::Store,
    tui,
    wacli::Wacli,
};
use serde_json::Value;
use tokio::time::sleep;

#[derive(Parser)]
#[command(
    name = "orbit",
    version,
    about = "Local-first WhatsApp CLI and durable message mirror"
)]
struct Cli {
    /// Print machine-readable JSON.
    #[arg(long, global = true)]
    json: bool,
    /// Override ~/.orbit (useful for isolated profiles).
    #[arg(long, global = true)]
    home: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Open Orbit's interactive local WhatsApp control surface.
    Ui {
        /// Open with fictional local data for screenshots and evaluation.
        #[arg(long)]
        demo: bool,
    },
    /// Initialize storage and install the checksum-verified wacli driver.
    Setup {
        #[arg(long)]
        force_driver: bool,
        #[arg(long)]
        wacli_path: Option<PathBuf>,
    },
    /// Pair a WhatsApp linked device by QR code.
    Connect {
        #[arg(value_enum, default_value = "whatsapp")]
        connector: Connector,
    },
    /// Show daemon, connector, and index state.
    Status,
    /// Run layered diagnostics.
    Doctor,
    /// Show local storage counts and size.
    Stats,
    /// Search the normalized local index.
    Search(SearchArgs),
    /// Manage the background daemon.
    #[command(subcommand)]
    Daemon(DaemonCommand),
    /// Operate WhatsApp.
    #[command(subcommand)]
    Whatsapp(WhatsappCommand),
}

#[derive(Clone, clap::ValueEnum)]
enum Connector {
    Whatsapp,
}

#[derive(Subcommand)]
enum DaemonCommand {
    Start,
    Run,
    Stop,
    Status,
}

#[derive(Args)]
struct SearchArgs {
    query: String,
    #[arg(long)]
    chat: Option<String>,
    #[arg(long)]
    from: Option<String>,
    #[arg(long)]
    after: Option<String>,
    #[arg(long)]
    before: Option<String>,
    #[arg(long, default_value_t = 50)]
    limit: u32,
}

#[derive(Subcommand)]
enum WhatsappCommand {
    Chats {
        #[arg(long, default_value_t = 50)]
        limit: u32,
    },
    Contacts {
        #[arg(default_value = "")]
        query: String,
        #[arg(long, default_value_t = 50)]
        limit: u32,
    },
    Unread {
        #[arg(long, default_value_t = 50)]
        limit: u32,
    },
    Messages {
        chat: String,
        #[arg(long, default_value_t = 50)]
        limit: u32,
    },
    Search(SearchArgs),
    Send(SendArgs),
    Download {
        message_id: String,
        #[arg(long)]
        chat: String,
    },
    /// Force immediate reliable reconciliation.
    Reconcile,
}

#[derive(Args)]
struct SendArgs {
    to: String,
    /// Text message. Omit when sending an attachment.
    message: Option<String>,
    #[arg(long, value_name="PATH", conflicts_with_all=["image","video","audio","voice"])]
    file: Option<PathBuf>,
    #[arg(long, value_name="PATH", conflicts_with_all=["file","video","audio","voice"])]
    image: Option<PathBuf>,
    #[arg(long, value_name="PATH", conflicts_with_all=["file","image","audio","voice"])]
    video: Option<PathBuf>,
    #[arg(long, value_name="PATH", conflicts_with_all=["file","image","video","voice"])]
    audio: Option<PathBuf>,
    #[arg(long, value_name="PATH", conflicts_with_all=["file","image","video","audio"])]
    voice: Option<PathBuf>,
    #[arg(long)]
    caption: Option<String>,
}

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("Error: {error:#}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let cli = Cli::parse();
    let paths = cli
        .home
        .clone()
        .map_or_else(OrbitPaths::discover, |root| Ok(OrbitPaths::for_root(root)))?;
    match cli.command {
        Command::Ui { demo } => {
            if cli.json {
                bail!("--json cannot be combined with the interactive `ui` command");
            }
            // Demo mode is an in-memory, non-sending visual sandbox. Keeping it
            // independent of setup makes documentation capture and terminal
            // compatibility checks safe on clean machines.
            if !demo {
                ensure_initialized(&paths)?;
            }
            tui::run(&paths, demo).await
        }
        Command::Setup {
            force_driver,
            wacli_path,
        } => setup(&paths, force_driver, wacli_path, cli.json).await,
        Command::Connect {
            connector: Connector::Whatsapp,
        } => connect(&paths).await,
        Command::Daemon(command) => daemon_command(&paths, command, cli.json).await,
        Command::Status => send_and_print(&paths, Request::Status, cli.json).await,
        Command::Doctor => send_and_print(&paths, Request::Doctor, cli.json).await,
        Command::Stats => send_and_print(&paths, Request::Stats, cli.json).await,
        Command::Search(args) => send_and_print(&paths, search_request(args), cli.json).await,
        Command::Whatsapp(command) => whatsapp(&paths, command, cli.json).await,
    }
}

async fn setup(
    paths: &OrbitPaths,
    force: bool,
    override_path: Option<PathBuf>,
    json: bool,
) -> Result<()> {
    paths.create()?;
    let mut config = Config::load(paths)?;
    if let Some(path) = override_path {
        if !path.is_file() {
            bail!("--wacli-path is not a file: {}", path.display());
        }
        config.wacli_path = Some(std::fs::canonicalize(path)?);
    } else {
        install::install_wacli(paths, force).await?;
        config.wacli_path = None;
    }
    config.save(paths)?;
    Store::new(paths.database.clone()).initialize()?;
    let driver = Wacli::new(config.resolved_wacli(paths), paths.whatsapp_store.clone());
    driver.ensure_compatible().await?;
    print_value(
        &serde_json::json!({"setup":"complete","root":paths.root,"database":paths.database,"wacli":driver.version().await?}),
        json,
    );
    Ok(())
}

async fn connect(paths: &OrbitPaths) -> Result<()> {
    ensure_initialized(paths)?;
    // Pairing and continuous sync must not compete for the same wacli store lock.
    if ipc::request(&paths.ipc_name(), &Request::Ping)
        .await
        .is_ok()
    {
        bail!("stop the daemon with `orbit daemon stop` before pairing");
    }
    let config = Config::load(paths)?;
    Wacli::new(config.resolved_wacli(paths), paths.whatsapp_store.clone())
        .authenticate()
        .await
}

async fn daemon_command(paths: &OrbitPaths, command: DaemonCommand, json: bool) -> Result<()> {
    match command {
        DaemonCommand::Run => {
            ensure_initialized(paths)?;
            orbit_core::daemon::run(paths.clone(), Config::load(paths)?).await
        }
        DaemonCommand::Start => start_daemon(paths, json).await,
        DaemonCommand::Stop => send_and_print(paths, Request::Shutdown, json).await,
        DaemonCommand::Status => send_and_print(paths, Request::Ping, json).await,
    }
}

async fn start_daemon(paths: &OrbitPaths, json: bool) -> Result<()> {
    ensure_initialized(paths)?;
    if ipc::request(&paths.ipc_name(), &Request::Ping)
        .await
        .is_ok()
    {
        print_value(&serde_json::json!({"daemon":"already_running"}), json);
        return Ok(());
    }
    let current = std::env::current_exe()?;
    let daemon_name = if cfg!(windows) {
        "orbitd.exe"
    } else {
        "orbitd"
    };
    let daemon = current.with_file_name(daemon_name);
    if !daemon.is_file() {
        bail!(
            "orbitd is missing next to {}; install both binaries",
            current.display()
        );
    }
    let log_path = paths.logs.join("orbitd.log");
    let stdout = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    let stderr = stdout.try_clone()?;
    let mut command = std::process::Command::new(daemon);
    command
        .arg("--home")
        .arg(&paths.root)
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr);
    configure_detached(&mut command);
    command.spawn().context("start Orbit daemon")?;
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if ipc::request(&paths.ipc_name(), &Request::Ping)
            .await
            .is_ok()
        {
            print_value(
                &serde_json::json!({"daemon":"started","log":log_path}),
                json,
            );
            return Ok(());
        }
        sleep(Duration::from_millis(100)).await;
    }
    bail!(
        "daemon did not become ready; inspect {}",
        log_path.display()
    )
}

#[cfg(windows)]
fn configure_detached(command: &mut std::process::Command) {
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::System::Threading::{CREATE_NEW_PROCESS_GROUP, DETACHED_PROCESS};
    command.creation_flags(CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS);
}

#[cfg(not(windows))]
fn configure_detached(_command: &mut std::process::Command) {}

async fn whatsapp(paths: &OrbitPaths, command: WhatsappCommand, json: bool) -> Result<()> {
    let request = match command {
        WhatsappCommand::Chats { limit } => Request::Chats {
            unread_only: false,
            limit,
        },
        WhatsappCommand::Contacts { query, limit } => Request::Contacts { query, limit },
        WhatsappCommand::Unread { limit } => Request::Chats {
            unread_only: true,
            limit,
        },
        WhatsappCommand::Messages { chat, limit } => Request::Messages { chat, limit },
        WhatsappCommand::Search(args) => search_request(args),
        WhatsappCommand::Download { message_id, chat } => Request::Download { message_id, chat },
        WhatsappCommand::Reconcile => Request::Reconcile,
        WhatsappCommand::Send(args) => send_request(args)?,
    };
    send_and_print(paths, request, json).await
}

fn search_request(args: SearchArgs) -> Request {
    Request::Search {
        query: args.query,
        chat: args.chat,
        from: args.from,
        after: args.after,
        before: args.before,
        limit: args.limit,
    }
}

fn send_request(args: SendArgs) -> Result<Request> {
    let candidates = [
        args.file.map(|p| (p, None, false)),
        args.image.map(|p| (p, Some("image".into()), false)),
        args.video.map(|p| (p, Some("video".into()), false)),
        args.audio.map(|p| (p, Some("audio".into()), false)),
        args.voice.map(|p| (p, None, true)),
    ];
    if let Some((path, media_as, voice)) = candidates.into_iter().flatten().next() {
        if args.message.is_some() {
            bail!(
                "provide either MESSAGE or an attachment, not both; use --caption for media text"
            );
        }
        return Ok(Request::SendFile {
            to: args.to,
            path: path.to_string_lossy().into_owned(),
            caption: args.caption,
            media_as,
            voice,
        });
    }
    if args.caption.is_some() {
        bail!("--caption requires an attachment");
    }
    let message = args
        .message
        .context("MESSAGE or an attachment option is required")?;
    Ok(Request::SendText {
        to: args.to,
        message,
    })
}

async fn send_and_print(paths: &OrbitPaths, request: Request, json: bool) -> Result<()> {
    let response = ipc::request(&paths.ipc_name(), &request).await?;
    if !response.ok {
        bail!(
            "{}",
            response
                .error
                .unwrap_or_else(|| "unknown daemon error".into())
        );
    }
    print_response(response, json);
    Ok(())
}

fn print_response(response: Response, json_output: bool) {
    if let Some(data) = response.data {
        print_value(&data, json_output);
    }
    if let Some(warning) = response.warning {
        eprintln!("Warning: {warning}");
    }
}

fn print_value(value: &Value, _json_output: bool) {
    // Pretty JSON is deterministic, scriptable, and does not truncate message IDs.
    // A future table renderer can be added without changing the daemon protocol.
    println!(
        "{}",
        serde_json::to_string_pretty(value).expect("serialize response")
    );
}

fn ensure_initialized(paths: &OrbitPaths) -> Result<()> {
    if !paths.config.is_file() || !paths.database.is_file() {
        bail!("Orbit is not initialized; run `orbit setup`");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attachment_and_text_are_mutually_exclusive_at_policy_layer() {
        let args = SendArgs {
            to: "Alex".into(),
            message: Some("hello".into()),
            file: None,
            image: Some("a.jpg".into()),
            video: None,
            audio: None,
            voice: None,
            caption: None,
        };
        assert!(send_request(args).is_err());
    }

    #[test]
    fn image_maps_to_explicit_wacli_media_type() {
        let args = SendArgs {
            to: "Alex".into(),
            message: None,
            file: None,
            image: Some("a.jpg".into()),
            video: None,
            audio: None,
            voice: None,
            caption: Some("look".into()),
        };
        match send_request(args).unwrap() {
            Request::SendFile {
                media_as, caption, ..
            } => {
                assert_eq!(media_as.as_deref(), Some("image"));
                assert_eq!(caption.as_deref(), Some("look"));
            }
            _ => panic!("wrong request"),
        }
    }

    #[test]
    fn ui_is_a_first_class_cli_command() {
        let cli = Cli::try_parse_from(["orbit", "ui"]).unwrap();
        assert!(matches!(cli.command, Command::Ui { demo: false }));
        assert!(!cli.json);
    }
}
