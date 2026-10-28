//! The `voice` command: `voice transcribe <file>` sends one audio file through the configured online transcription
//! backend and prints the transcript.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use charliebot_agents::cli_host::LazyHost;
use charliebot_cli_common::args::with_infer_long_args;
use charliebot_cli_common::exit::CliExit;
use charliebot_cli_common::http;
use charliebot_core::{credentials, home};
use clap::{Args, CommandFactory, FromArgMatches, Parser, Subcommand};

#[derive(Parser)]
#[command(name = "voice", subcommand_required = true, arg_required_else_help = true)]
struct VoiceCli {
  #[command(subcommand)]
  command: VoiceCommand,
}

#[derive(Subcommand)]
enum VoiceCommand {
  /// Transcribe one audio file through voice.default_backend and print the transcript.
  Transcribe(TranscribeArgs),
}

#[derive(Args)]
struct TranscribeArgs {
  /// Audio in a container the decoder reads, or raw 16 kHz mono s16 PCM (.pcm, .s16).
  file: PathBuf,
}

/// Run the `voice` subcommand.
pub fn main(args: &[OsString], lazy_host: &LazyHost) -> Result<(), CliExit> {
  let command = with_infer_long_args(VoiceCli::command());
  let argv = std::iter::once(OsString::from("charliebot voice")).chain(args.iter().cloned());
  let matches = match command.try_get_matches_from(argv) {
    Ok(matches) => matches,
    Err(error) => error.exit(),
  };
  let cli = VoiceCli::from_arg_matches(&matches).expect("try_get_matches_from validated the arguments");
  match cli.command {
    VoiceCommand::Transcribe(args) => transcribe(&args.file, lazy_host),
  }
}

fn transcribe(file: &Path, lazy_host: &LazyHost) -> Result<(), CliExit> {
  let host = lazy_host.get().map_err(|error| CliExit::error(format!("{error:#}")))?;
  let home = home::charliebot_home_dir().map_err(|error| CliExit::error(error.to_string()))?;
  let credentials = credentials::load_credentials(&home).map_err(|error| CliExit::error(error.to_string()))?;
  http::ensure_crypto_provider();
  let runtime = tokio::runtime::Builder::new_current_thread()
    .enable_all()
    .build()
    .map_err(|error| CliExit::error(error.to_string()))?;
  let transcript =
    runtime.block_on(charliebot_voice::transcribe_file(file, &host.cfg, &credentials)).map_err(CliExit::error)?;
  println!("{transcript}");
  Ok(())
}
