use agent_client_protocol::{BoxFuture, Channel, ConnectTo, Error, RawJsonRpcMessage, Role};
use futures_util::StreamExt;
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, BufWriter,
};

const CLEAN_EOF: &str = "uri-agent:acp-stdin-eof";
pub(super) const MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;

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
            let mut incoming = BufReader::new(self.incoming);
            let mut outgoing = BufWriter::new(self.outgoing);
            let mut line = Vec::new();
            loop {
                tokio::select! {
                    read = read_line_limited(&mut incoming, &mut line, MAX_MESSAGE_BYTES) => match read {
                        Ok(0) => return Err(Error::internal_error().data(CLEAN_EOF)),
                        Ok(_) => {
                            let message = match serde_json::from_slice::<RawJsonRpcMessage>(&line) {
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
                        Err(error) if error.kind() == std::io::ErrorKind::InvalidData => {
                            return Err(Error::internal_error().data(format!(
                                "ACP stdio message exceeds {MAX_MESSAGE_BYTES} bytes"
                            )));
                        }
                        Err(error) => return Err(Error::into_internal_error(error)),
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

async fn read_line_limited<R>(
    reader: &mut R,
    line: &mut Vec<u8>,
    max_message_bytes: usize,
) -> std::io::Result<usize>
where
    R: AsyncBufRead + Unpin,
{
    line.clear();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(line.len());
        }
        let take = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |index| index + 1);
        if line.len().saturating_add(take) > max_message_bytes {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "ACP stdio message is too large",
            ));
        }
        let complete = available.get(take.saturating_sub(1)) == Some(&b'\n');
        line.extend_from_slice(&available[..take]);
        reader.consume(take);
        if complete {
            return Ok(line.len());
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn line_reader_rejects_messages_over_its_limit() {
        let input = b"12345\n".as_slice();
        let mut reader = BufReader::new(input);
        let mut line = Vec::new();

        let error = read_line_limited(&mut reader, &mut line, 4)
            .await
            .unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(line.len() <= 4);
    }
}
