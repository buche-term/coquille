//! coquille (`cq`): a shell for the buche terminal. It speaks buche's driver
//! protocol (JSON lines) on its control channel (fd $BUCHE_CONTROL_FD), or on
//! stdin/stdout.

use std::path::PathBuf;

use clap::Parser;
use coquille::Shell;
use coquille::channel::{control_fd_var, json_lines};
use coquille::fdio::{FdStream, dup_cloexec};
use coquille::protocol::SELF;
use coquille::util::terminal_size;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

#[derive(Parser)]
#[command(name = "cq", version, about = "A shell for the Buche terminal")]
struct Cli {
    /// Run a command and exit
    #[arg(short = 'c', value_name = "COMMAND")]
    command: Option<String>,

    /// Read instructions from a JSONL file instead of running a command
    #[arg(long, value_name = "FILE")]
    play: Option<PathBuf>,

    /// Output to stdout instead of the control file descriptor (fd 5)
    #[arg(long)]
    stdout: bool,
}

/// Read JSON lines from `input` into `tx` (lines that aren't JSON are
/// skipped).
async fn read_messages(input: impl AsyncRead + Unpin, tx: mpsc::UnboundedSender<Value>) {
    json_lines(input, |message| std::future::ready(tx.send(message).is_ok())).await
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    let control_fd = control_fd_var();

    // The control fd is used for output (unless --stdout is given) and for
    // input (unless --play or -c is given).
    let control_output = control_fd.filter(|_| !cli.stdout);
    let control_input = control_fd.filter(|_| cli.play.is_none() && cli.command.is_none());

    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Value>();
    let mut output: Box<dyn AsyncWrite + Unpin> = match control_output {
        Some(fd) => Box::new(FdStream::new(dup_cloexec(fd)?)?),
        None => Box::new(tokio::io::stdout()),
    };
    let writer = tokio::task::spawn_local(async move {
        while let Some(message) = out_rx.recv().await {
            let mut line = serde_json::to_vec(&message).unwrap_or_default();
            line.push(b'\n');
            if output.write_all(&line).await.is_err() || output.flush().await.is_err() {
                return;
            }
        }
    });

    let (in_tx, in_rx) = mpsc::unbounded_channel::<Value>();
    if let Some(text) = cli.command {
        // Submit the command the way the terminal would: a `command` message
        // for the shell itself.
        let _ = in_tx.send(json!({ "type": "command", "action": { "builtin": "submit" }, "to": SELF, "text": text }));
        drop(in_tx); // (Nothing else comes.)
    } else if let Some(play) = cli.play {
        let file = tokio::fs::File::open(&play)
            .await
            .map_err(|e| anyhow::anyhow!("{}: {e}", play.display()))?;
        tokio::task::spawn_local(read_messages(file, in_tx));
    } else if let Some(fd) = control_input {
        tokio::task::spawn_local(read_messages(FdStream::new(dup_cloexec(fd)?)?, in_tx));
    } else {
        tokio::task::spawn_local(read_messages(tokio::io::stdin(), in_tx));
    }

    let (cols, rows) = terminal_size(libc::STDOUT_FILENO);
    let shell = Shell::new(out_tx, cols, rows);
    shell.run(in_rx).await;
    let _ = writer.await;
    Ok(())
}

fn main() {
    let cli = Cli::parse();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let local = tokio::task::LocalSet::new();
    match local.block_on(&runtime, run(cli)) {
        // (Without waiting for what is still running: daemons, a stuck
        // reader...)
        Ok(()) => std::process::exit(0),
        Err(err) => {
            eprintln!("cq: {err:#}");
            std::process::exit(1);
        }
    }
}
