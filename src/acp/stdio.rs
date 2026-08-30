use agent_client_protocol::{BoxFuture, Channel, ConnectTo, Error, RawJsonRpcMessage, Role};
use futures_util::StreamExt;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, BufWriter};

const CLEAN_EOF: &str = "uri-agent:acp-stdin-eof";

/// Newline-delimited JSON transport that treats stdin EOF as a clean shutdown.
pub(super) struct AcpStdio<Outgoing, Incoming> {
    outgoing: Outgoing,
    incoming: Incoming,
}

impl AcpStdio<tokio::io::Stdout, tokio::io::Stdin> {
    pub(super) fn stdio() -> Self {
        Self::new(tokio::io::stdout(), tokio::io::stdin())
    }
}

impl<Outgoing, Incoming> AcpStdio<Outgoing, Incoming> {
    pub(super) fn new(outgoing: Outgoing, incoming: Incoming) -> Self {
        Self { outgoing, incoming }
    }
}

impl<Outgoing, Incoming, Counterpart> ConnectTo<Counterpart> for AcpStdio<Outgoing, Incoming>
where
    Outgoing: AsyncWrite + Unpin + Send + 'static,
    Incoming: AsyncRead + Unpin + Send + 'static,
    Counterpart: Role,
{
    async fn connect_to(
        self,
        client: impl ConnectTo<Counterpart::Counterpart>,
    ) -> agent_client_protocol::Result<()> {
        let (channel, transport) = <Self as ConnectTo<Counterpart>>::into_channel_and_future(self);
        tokio::select! {
            result = client.connect_to(channel) => result,
            result = transport => result,
        }
    }

    fn into_channel_and_future(
        self,
    ) -> (
        Channel,
        BoxFuture<'static, agent_client_protocol::Result<()>>,
    ) {
        let (caller, mut transport) = Channel::duplex();
        let future = Box::pin(async move {
            let mut incoming = BufReader::new(self.incoming).lines();
            let mut outgoing = BufWriter::new(self.outgoing);
            loop {
                tokio::select! {
                    line = incoming.next_line() => match line.map_err(Error::into_internal_error)? {
                        Some(line) => {
                            let message = match serde_json::from_str::<RawJsonRpcMessage>(&line) {
                                Ok(message) => Ok(message),
                                // Do not echo malformed input: session setup messages may
                                // contain literal MCP credentials.
                                Err(_) => Err(Error::parse_error()),
                            };
                            transport
                                .tx
                                .unbounded_send(message)
                                .map_err(Error::into_internal_error)?;
                        }
                        None => return Err(Error::internal_error().data(CLEAN_EOF)),
                    },
                    message = transport.rx.next() => match message {
                        Some(message) => {
                            let line = serde_json::to_vec(&message?)
                                .map_err(Error::into_internal_error)?;
                            outgoing
                                .write_all(&line)
                                .await
                                .map_err(Error::into_internal_error)?;
                            outgoing
                                .write_all(b"\n")
                                .await
                                .map_err(Error::into_internal_error)?;
                            outgoing.flush().await.map_err(Error::into_internal_error)?;
                        }
                        None => return Ok(()),
                    },
                }
            }
        });
        (caller, future)
    }
}

pub(super) fn is_clean_eof(error: &Error) -> bool {
    fn contains_marker(value: &serde_json::Value) -> bool {
        match value {
            serde_json::Value::String(value) => value == CLEAN_EOF,
            serde_json::Value::Array(values) => values.iter().any(contains_marker),
            serde_json::Value::Object(values) => values.values().any(contains_marker),
            _ => false,
        }
    }

    error.data.as_ref().is_some_and(contains_marker)
}
