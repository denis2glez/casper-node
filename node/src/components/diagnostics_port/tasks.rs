use std::{
    borrow::Cow,
    fmt::{self, Debug, Display, Formatter},
    fs, io,
    path::PathBuf,
};

use bincode::{
    config::{AllowTrailing, FixintEncoding, WithOtherIntEncoding, WithOtherTrailing},
    DefaultOptions, Options,
};
use erased_serde::Serializer as ErasedSerializer;
use futures::future::{self, Either};
use serde::Serialize;
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader},
    net::{unix::OwnedWriteHalf, UnixListener, UnixStream},
    sync::watch,
};
use tracing::{debug, info, info_span, warn, Instrument};

use casper_types::EraId;

use super::{
    command::{Action, Command, OutputFormat},
    util::ShowUnixAddr,
};
use crate::{
    components::consensus::EraDump,
    effect::{
        announcements::{ControlAnnouncement, QueueDumpFormat},
        diagnostics_port::DumpConsensusStateRequest,
        EffectBuilder,
    },
};

/// Success or failure response.
///
/// This response is sent back to clients after every operation (unless suppressed in quiet mode),
/// indicating the outcome of the operation.
#[derive(Debug, Serialize)]
enum Outcome {
    /// Operation succeeded.
    Success {
        /// Human-readable message giving additional info and/or stating the effect.
        msg: String,
    },
    /// Operation failed.
    Failure {
        /// Human-readable message describing the failure that occurred.
        reason: String,
    },
}

impl Outcome {
    /// Constructs a new successful outcome.
    fn success<S: ToString>(msg: S) -> Self {
        Outcome::Success {
            msg: msg.to_string(),
        }
    }

    /// Constructs a new failed outcome.
    fn failed<S: ToString>(reason: S) -> Self {
        Outcome::Failure {
            reason: reason.to_string(),
        }
    }
}

impl Display for Outcome {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Outcome::Success { msg } => {
                write!(f, "OK   {}", msg)
            }
            Outcome::Failure { reason } => {
                write!(f, "ERR  {}", reason)
            }
        }
    }
}

/// Configuration for a connection diagnostics port session.
#[derive(Copy, Clone, Debug, Default, Serialize)]
struct Session {
    /// Whether or not to suppress the operation outcome.
    quiet: bool,
    /// Output format to send to client.
    output: OutputFormat,
}

impl Display for Session {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        Debug::fmt(self, f)
    }
}

pub enum TempFileSerializer {
    Json(serde_json::Serializer<fs::File>),
    Bincode(
        bincode::Serializer<
            fs::File,
            WithOtherTrailing<WithOtherIntEncoding<DefaultOptions, FixintEncoding>, AllowTrailing>,
        >,
    ),
}

impl TempFileSerializer {
    /// Converts the temp file serializer into an actual erased serializer.
    pub fn as_serializer<'a>(&'a mut self) -> Box<dyn ErasedSerializer + 'a> {
        match self {
            TempFileSerializer::Json(json) => Box::new(<dyn erased_serde::Serializer>::erase(json)),
            TempFileSerializer::Bincode(bincode) => {
                Box::new(<dyn erased_serde::Serializer>::erase(bincode))
            }
        }
    }
}

impl Session {
    /// Creates a serializer for an `EraDump`.
    fn create_era_dump_serializer(&self) -> fn(&EraDump<'_>) -> Result<Vec<u8>, Cow<'static, str>> {
        // TODO: This function could probably be a generic serialization function for any `T`, but
        // the conversion is tricky due to the lifetime arguments on `EraDump` and has not been done
        // yet.
        match self.output {
            OutputFormat::Interactive => |data: &EraDump| {
                let mut buf = data.to_string().into_bytes();
                buf.push(b'\n');
                Ok(buf)
            },
            OutputFormat::Json => |data: &EraDump| {
                let mut buf = serde_json::to_vec(&data).map_err(|err| {
                    Cow::Owned(format!("failed to serialize era dump as JSON: {}", err))
                })?;
                buf.push(b'\n');
                Ok(buf)
            },
            OutputFormat::Bincode => |data: &EraDump| {
                bincode::serialize(&data).map_err(|err| {
                    Cow::Owned(format!("failed to serialize era dump as bincode: {}", err))
                })
            },
        }
    }

    /// Creates a generic serializer that is writing to a temporary file.
    ///
    /// The resulting serializer will write to the given file.
    fn create_queue_dump_format(&self, file: fs::File) -> QueueDumpFormat {
        match self.output {
            OutputFormat::Interactive => QueueDumpFormat::debug(file),
            OutputFormat::Json => {
                QueueDumpFormat::serde(TempFileSerializer::Json(serde_json::Serializer::new(file)))
            }
            OutputFormat::Bincode => {
                QueueDumpFormat::serde(TempFileSerializer::Bincode(bincode::Serializer::new(
                    file,
                    // TODO: Do not use `bincode::serialize` above, but rather always instantiate
                    // options across the file to ensure it is always the same.
                    DefaultOptions::new()
                        .with_fixint_encoding()
                        .allow_trailing_bytes(),
                )))
            }
        }
    }

    /// Processes a single command line sent from a client.
    async fn process_line<REv>(
        &mut self,
        effect_builder: EffectBuilder<REv>,
        writer: &mut OwnedWriteHalf,
        line: &str,
    ) -> io::Result<bool>
    where
        REv: From<DumpConsensusStateRequest> + From<ControlAnnouncement> + Send,
    {
        debug!(%line, "line received");
        match Command::from_line(line) {
            Ok(ref cmd) => {
                info!(?cmd, "processing command");
                match cmd.action {
                    Action::Session => {
                        self.send_outcome(writer, &Outcome::success("showing session info"))
                            .await?;
                        self.send_to_client(writer, &self).await?;
                    }
                    Action::Set { quiet, output } => {
                        let mut changed = false;

                        if let Some(quiet) = quiet {
                            changed |= self.quiet != quiet;
                            self.quiet = quiet;
                        }

                        if let Some(output) = output {
                            changed |= self.output != output;
                            self.output = output;
                        }

                        if changed {
                            self.send_outcome(writer, &Outcome::success("session updated"))
                                .await?;
                        } else {
                            self.send_outcome(writer, &Outcome::success("session unchanged"))
                                .await?;
                        }
                    }
                    Action::DumpConsensus { era } => {
                        let output = effect_builder
                            .diagnostics_port_dump_consensus_state(
                                era.map(EraId::new),
                                self.create_era_dump_serializer(),
                            )
                            .await;

                        match output {
                            Ok(ref data) => {
                                self.send_outcome(
                                    writer,
                                    &Outcome::success("dumping consensus state"),
                                )
                                .await?;
                                writer.write_all(data).await?;
                            }
                            Err(err) => {
                                self.send_outcome(writer, &Outcome::failed(err)).await?;
                            }
                        }
                    }
                    Action::DumpQueues => {
                        let tempdir = tempfile::tempdir().expect("FIXME");
                        let tempfile_path = tempdir.path().join("queue-dump");

                        match fs::File::create(&tempfile_path) {
                            Ok(tmp) => {
                                effect_builder
                                    .diagnostics_port_dump_queue(self.create_queue_dump_format(tmp))
                                    .await;

                                // We can now reopen the file and send it.
                                match fs::File::open(tempfile_path) {
                                    Ok(reopened_tempfile) => {
                                        self.send_outcome(
                                            writer,
                                            &Outcome::success("dumping queues"),
                                        )
                                        .await?;

                                        // At this point, we have a complete queue dump in the
                                        // requested format stored inside the temporary file.
                                        let mut tokio_file =
                                            tokio::fs::File::from_std(reopened_tempfile);

                                        self.stream_to_client(writer, &mut tokio_file).await?;
                                    }
                                    Err(err) => {
                                        self.send_outcome(
                                            writer,
                                            &Outcome::failed(format!(
                                                "could not reopen temporary file: {}",
                                                err
                                            )),
                                        )
                                        .await?;
                                    }
                                }
                            }
                            Err(err) => {
                                self.send_outcome(
                                    writer,
                                    &Outcome::failed(format!(
                                        "could not create a temporary file for queue dump: {}",
                                        err
                                    )),
                                )
                                .await?;
                            }
                        };
                    }
                    Action::Quit => {
                        self.send_outcome(writer, &Outcome::success("goodbye!"))
                            .await?;
                        return Ok(false);
                    }
                };
            }
            Err(err) => {
                self.send_outcome(writer, &Outcome::failed(err.to_string().as_str()))
                    .await?
            }
        }

        Ok(true)
    }

    /// Sends an operation outcome.
    ///
    /// The outcome will be silently dropped if the session is in quiet mode.
    async fn send_outcome(
        &self,
        writer: &mut OwnedWriteHalf,
        response: &Outcome,
    ) -> io::Result<()> {
        if self.quiet {
            return Ok(());
        }

        self.send_to_client(writer, response).await
    }

    /// Sends a message to the client.
    ///
    /// Any type of message can be sent to a client, as long as it has a `Display` (use for
    /// `interactive` encoding) and `Serialize` (used for `bincode` and `json`) implementation.
    async fn send_to_client<T>(&self, writer: &mut OwnedWriteHalf, response: &T) -> io::Result<()>
    where
        T: Display + Serialize,
    {
        match self.output {
            OutputFormat::Interactive => {
                writer.write_all(response.to_string().as_bytes()).await?;
                writer.write_all(b"\n").await?;
            }
            OutputFormat::Json => {
                let buf = serde_json::to_string_pretty(response)
                    .map_err(|err| io::Error::new(io::ErrorKind::Other, err))?;
                writer.write_all(buf.as_bytes()).await?;
                writer.write_all(b"\n").await?;
            }
            OutputFormat::Bincode => {
                let buf = bincode::serialize(response)
                    .map_err(|err| io::Error::new(io::ErrorKind::Other, err))?;
                writer.write_all(&buf).await?;
            }
        }

        Ok(())
    }

    /// Streams data from a source to the client.
    ///
    /// Returns the number of bytes sent.
    async fn stream_to_client<R: AsyncRead + Unpin + ?Sized>(
        &self,
        writer: &mut OwnedWriteHalf,
        src: &mut R,
    ) -> io::Result<u64> {
        tokio::io::copy(src, writer).await
    }
}

/// Handler for client connection.
///
/// The core loop for the diagnostics port; reads commands via unix socket and processes them.
///
/// # Security
///
/// The handler itself will buffer an unlimited amount of data if no newline is encountered in the
/// input stream. For this reason ensure that only trusted client connect to the socket producing
/// the passed in `stream`.
async fn handler<REv>(
    effect_builder: EffectBuilder<REv>,
    stream: UnixStream,
    mut shutdown_receiver: watch::Receiver<()>,
) -> io::Result<()>
where
    REv: From<DumpConsensusStateRequest> + From<ControlAnnouncement> + Send,
{
    debug!("accepted new connection on diagnostics port");

    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    let mut session = Session::default();

    let mut keep_going = true;
    while keep_going {
        let shutdown_messages = async { while shutdown_receiver.changed().await.is_ok() {} };

        match future::select(Box::pin(shutdown_messages), Box::pin(lines.next_line())).await {
            Either::Left(_) => {
                info!("shutting down diagnostics port connection to client");
                return Ok(());
            }
            Either::Right((line_result, _)) => {
                if let Some(line) = line_result? {
                    keep_going = session
                        .process_line(effect_builder, &mut writer, line.as_str())
                        .await?;
                } else {
                    info!("client closed diagnostics port connection");
                    return Ok(());
                }
            }
        }
    }

    Ok(())
}

/// Server task for diagnostics port.
pub(super) async fn server<REv>(
    effect_builder: EffectBuilder<REv>,
    socket_path: PathBuf,
    listener: UnixListener,
    mut shutdown_receiver: watch::Receiver<()>,
) where
    REv: From<DumpConsensusStateRequest> + From<ControlAnnouncement> + Send,
{
    let handling_shutdown_receiver = shutdown_receiver.clone();
    let mut next_client_id: u64 = 0;
    let accept_connections = async move {
        loop {
            match listener.accept().await {
                Ok((stream, client_addr)) => {
                    let client_id = next_client_id;

                    let span = info_span!("diagnostics_port", client_id,);

                    span.in_scope(|| {
                        info!(client_addr = %ShowUnixAddr(&client_addr), "accepted connection");
                    });

                    next_client_id += 1;

                    tokio::spawn(
                        handler(effect_builder, stream, handling_shutdown_receiver.clone())
                            .instrument(span),
                    );
                }
                Err(err) => {
                    info!(%err, "failed to accept incoming connection on diagnostics port");
                }
            }
        }
    };

    let shutdown_messages = async move { while shutdown_receiver.changed().await.is_ok() {} };

    // Now we can wait for either the `shutdown` channel's remote end to do be dropped or the
    // infinite loop to terminate, which never happens.
    match future::select(Box::pin(shutdown_messages), Box::pin(accept_connections)).await {
        Either::Left(_) => info!("shutting down diagnostics port"),
        Either::Right(_) => unreachable!("server accept returns `!`"),
    }

    // When we're shutting down, we try to delete the socket, but only warn in case of failure.
    match fs::remove_file(&socket_path) {
        Ok(_) => {
            debug!(socket_path=%socket_path.display(), "removed socket file");
        }
        Err(_) => {
            warn!(socket_path=%socket_path.display(), "could not remove socket file");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        path::{Path, PathBuf},
        sync::Arc,
    };

    use crate::{
        components::diagnostics_port::Config as DiagnosticsPortConfig,
        testing::{
            self,
            network::{Network, NetworkedReactor},
            TestRng,
        },
        WithDir,
    };
    use casper_node_macros::reactor;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::UnixStream,
        sync::Notify,
    };

    pub struct TestReactorConfig {
        base_dir: PathBuf,
        diagnostics_port: DiagnosticsPortConfig,
    }

    impl TestReactorConfig {
        /// Creates a new test reactor configuration with a given base dir and index.
        fn new<P: AsRef<Path>>(base_dir: P, idx: usize) -> Self {
            TestReactorConfig {
                base_dir: base_dir.as_ref().to_owned(),
                diagnostics_port: DiagnosticsPortConfig {
                    enabled: true,
                    socket_path: format!("node_{}.socket", idx).into(),
                    socket_umask: 0o022,
                },
            }
        }

        fn socket_path(&self) -> PathBuf {
            self.base_dir.join(&self.diagnostics_port.socket_path)
        }
    }

    // For testing we use a tiny reactor that runs nothing but the diagnostics console:
    reactor!(Reactor {
        type Config = TestReactorConfig;

        components: {
            diagnostics_console = has_effects DiagnosticsPort(&WithDir::new(cfg.base_dir.clone(), cfg.diagnostics_port), event_queue);
        }

        events: {}

        requests: {
            DumpConsensusStateRequest -> !;
        }

        announcements: {}
    });

    impl NetworkedReactor for Reactor {}

    #[tokio::test]
    async fn ensure_diagnostics_port_can_dump_events() {
        testing::init_logging();

        let mut network = Network::<Reactor>::new();
        let mut rng = TestRng::new();

        let base_dir = tempfile::tempdir().expect("could not create tempdir");

        // We just add a single node to the network.
        let cfg = TestReactorConfig::new(base_dir.path(), 0);
        let socket_path = cfg.socket_path();
        let (_node_id, _runner) = network.add_node_with_config(cfg, &mut rng).await.unwrap();

        // Now crank, so that the listening socket is initialized.
        while 0 != network.crank_all(&mut rng).await {}

        let ready = Arc::new(Notify::new());

        // Start a background task that connects to the unix socket and sends a few requests down.
        let client_ready = ready.clone();
        let join_handle = tokio::spawn(async move {
            let mut stream = UnixStream::connect(socket_path)
                .await
                .expect("could not connect to socket path of node");

            // We put some dump-queues events on the stream, to have some events to dump when the
            // first one is processed.
            stream
                .write_all(b"set -o json -q false\ndump-queues\ndump-queues\ndump-queues\nquit\n")
                .await
                .expect("could not write to listener");
            stream.flush().await.expect("flushing failed");

            client_ready.notify_one();

            let mut buffer = Vec::new();
            stream
                .read_to_end(&mut buffer)
                .await
                .expect("could not read console output to end");

            String::from_utf8(buffer).expect("could not parse output as UTF8")
        });

        // Wait for all the commands to be buffered.
        ready.notified().await;

        // Now crank, so that the listening socket is initialized.
        while 0 != network.crank_all(&mut rng).await {
            eprintln!("crank");
        }

        eprintln!("finished cranking");

        let output = join_handle.await.expect("error joining client task");

        dbg!(output);
    }
}
