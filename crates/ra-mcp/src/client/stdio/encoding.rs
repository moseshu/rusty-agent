// Adapted from modelcontextprotocol/rust-sdk (Apache-2.0) @ 79437f291b2c44053d00dcd5db969fd0cca7c887, modified.
//! Text transcoding around the SDK byte transport; JSON-RPC remains entirely SDK-owned.

use super::McpEncodingErrorHandler;
use crate::client::safe_error;
use encoding_rs::{DecoderResult, EncoderResult, Encoding, UTF_16BE, UTF_16LE};
use ra_core::error::{Error, Result, ToolErrorKind};
use rmcp::{
    RoleClient,
    service::{RxJsonRpcMessage, TxJsonRpcMessage},
    transport::{Transport, async_rw::AsyncRwTransport},
};
use std::{
    io,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::io::{
    AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader, DuplexStream,
    ReadHalf, WriteHalf,
};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy)]
enum CodecKind {
    Standard(&'static Encoding),
    Ascii,
    Latin1,
}

pub(super) struct TextCodec {
    kind: CodecKind,
    handler: McpEncodingErrorHandler,
    utf16_bom: bool,
}
impl TextCodec {
    pub(super) fn new(label: &str, handler: McpEncodingErrorHandler) -> Result<Self> {
        let python = python_codec(label);
        let kind = match python {
            Some(PythonCodec::Ascii) => CodecKind::Ascii,
            Some(PythonCodec::Latin1) => CodecKind::Latin1,
            Some(PythonCodec::Utf8) => CodecKind::Standard(encoding_rs::UTF_8),
            Some(PythonCodec::Utf16) => CodecKind::Standard(if cfg!(target_endian = "little") {
                UTF_16LE
            } else {
                UTF_16BE
            }),
            Some(PythonCodec::Utf16Le) => CodecKind::Standard(UTF_16LE),
            Some(PythonCodec::Utf16Be) => CodecKind::Standard(UTF_16BE),
            None => CodecKind::Standard(
                Encoding::for_label(label.as_bytes())
                    .or_else(|| Encoding::for_label(label.replace('_', "-").as_bytes()))
                    .filter(|encoding| *encoding != encoding_rs::REPLACEMENT)
                    .ok_or_else(|| Error::config("unsupported MCP stdio text encoding"))?,
            ),
        };
        Ok(Self {
            kind,
            handler,
            utf16_bom: python == Some(PythonCodec::Utf16),
        })
    }
    async fn decode(
        &self,
        mut input: impl AsyncRead + Unpin,
        mut output: impl AsyncWrite + Unpin,
    ) -> io::Result<()> {
        let mut decoder = match self.kind {
            CodecKind::Standard(encoding) => Some(encoding.new_decoder_without_bom_handling()),
            _ => None,
        };
        if self.utf16_bom {
            // Python's incremental UTF-16 decoder: empty output is valid, one truncated byte
            // follows the error handler, and two bytes without a BOM fail under every handler.
            let mut marker = [0; 2];
            let mut filled = 0;
            while filled < marker.len() {
                let read = input.read(&mut marker[filled..]).await?;
                if read == 0 {
                    break;
                }
                filled += read;
            }
            let encoding = match &marker[..filled] {
                [0xff, 0xfe] => UTF_16LE,
                [0xfe, 0xff] => UTF_16BE,
                [] => return output.shutdown().await,
                [_] => {
                    match self.handler {
                        McpEncodingErrorHandler::Strict => return Err(codec_error()),
                        McpEncodingErrorHandler::Ignore => {}
                        McpEncodingErrorHandler::Replace => {
                            output.write_all("\u{fffd}".as_bytes()).await?;
                        }
                    }
                    return output.shutdown().await;
                }
                _ => return Err(codec_error()),
            };
            decoder = Some(encoding.new_decoder_without_bom_handling());
        }
        let mut bytes = [0; 8192];
        loop {
            let length = input.read(&mut bytes).await?;
            let mut text_buffer = String::with_capacity(32768);
            if let Some(decoder) = &mut decoder {
                let mut remaining = &bytes[..length];
                loop {
                    let (result, read) = decoder.decode_to_string_without_replacement(
                        remaining,
                        &mut text_buffer,
                        length == 0,
                    );
                    remaining = &remaining[read..];
                    match result {
                        DecoderResult::InputEmpty => break,
                        DecoderResult::OutputFull => {
                            output.write_all(text_buffer.as_bytes()).await?;
                            text_buffer.clear();
                        }
                        DecoderResult::Malformed(_, _) => match self.handler {
                            McpEncodingErrorHandler::Strict => return Err(codec_error()),
                            McpEncodingErrorHandler::Ignore => {}
                            McpEncodingErrorHandler::Replace => text_buffer.push('\u{fffd}'),
                        },
                    }
                }
            } else {
                for byte in &bytes[..length] {
                    if matches!(self.kind, CodecKind::Ascii) && !byte.is_ascii() {
                        match self.handler {
                            McpEncodingErrorHandler::Strict => return Err(codec_error()),
                            McpEncodingErrorHandler::Ignore => {}
                            McpEncodingErrorHandler::Replace => text_buffer.push('\u{fffd}'),
                        }
                    } else {
                        text_buffer.push(char::from(*byte));
                    }
                }
            }
            output.write_all(text_buffer.as_bytes()).await?;
            if length == 0 {
                return output.shutdown().await;
            }
        }
    }
    fn encode(&self, text: &str) -> io::Result<Vec<u8>> {
        let mut bytes = Vec::new();
        if let CodecKind::Standard(encoding) = self.kind {
            if encoding == UTF_16LE || encoding == UTF_16BE {
                for unit in text.encode_utf16() {
                    bytes.extend(if encoding == UTF_16LE {
                        unit.to_le_bytes()
                    } else {
                        unit.to_be_bytes()
                    });
                }
                return Ok(bytes);
            }
            let mut encoder = encoding.new_encoder();
            let mut remaining = text;
            loop {
                let mut buffer = [0; 8192];
                let (result, read, written) =
                    encoder.encode_from_utf8_without_replacement(remaining, &mut buffer, true);
                bytes.extend_from_slice(&buffer[..written]);
                remaining = &remaining[read..];
                match result {
                    EncoderResult::InputEmpty => return Ok(bytes),
                    EncoderResult::OutputFull => {}
                    EncoderResult::Unmappable(_) => match self.handler {
                        McpEncodingErrorHandler::Strict => return Err(codec_error()),
                        McpEncodingErrorHandler::Ignore => {}
                        McpEncodingErrorHandler::Replace => bytes.push(b'?'),
                    },
                }
            }
        }
        for character in text.chars() {
            let limit = if matches!(self.kind, CodecKind::Ascii) {
                127
            } else {
                255
            };
            if u32::from(character) <= limit {
                bytes.push(u8::try_from(u32::from(character)).map_err(|_| codec_error())?);
            } else {
                match self.handler {
                    McpEncodingErrorHandler::Strict => return Err(codec_error()),
                    McpEncodingErrorHandler::Ignore => {}
                    McpEncodingErrorHandler::Replace => bytes.push(b'?'),
                }
            }
        }
        Ok(bytes)
    }
    async fn encode_stream(
        &self,
        input: impl AsyncRead + Unpin,
        mut output: impl AsyncWrite + Unpin,
    ) -> io::Result<()> {
        let mut input = BufReader::new(input);
        let mut line = String::new();
        let mut first = true;
        loop {
            line.clear();
            if input.read_line(&mut line).await? == 0 {
                return output.shutdown().await;
            }
            // Python encodes each message separately, so its `utf-16` codec prefixes every line
            // with a BOM. One stream BOM is kept here: a later U+FEFF would precede the next JSON
            // value, which strict JSON line readers reject.
            if first && self.utf16_bom {
                output
                    .write_all(if cfg!(target_endian = "little") {
                        &[0xff, 0xfe]
                    } else {
                        &[0xfe, 0xff]
                    })
                    .await?;
            }
            first = false;
            output.write_all(&self.encode(&line)?).await?;
            output.flush().await?;
        }
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum PythonCodec {
    Ascii,
    Latin1,
    Utf8,
    Utf16,
    Utf16Le,
    Utf16Be,
}

// Python's `encodings` normalization and alias table for the codecs implemented here. These are
// resolved before `encoding_rs`, whose WHATWG labels map many ASCII/Latin-1 aliases to
// Windows-1252.
fn python_codec(label: &str) -> Option<PythonCodec> {
    let mut normalized = String::with_capacity(label.len());
    let mut pending_separator = false;
    for character in label.chars() {
        if character.is_ascii_alphanumeric() || character == '.' {
            if pending_separator && !normalized.is_empty() {
                normalized.push('_');
            }
            pending_separator = false;
            normalized.push(character.to_ascii_lowercase());
        } else {
            pending_separator = true;
        }
    }
    let dotted = normalized.replace('.', "_");
    [normalized.as_str(), dotted.as_str()]
        .into_iter()
        .find_map(|key| match key {
            "ascii" | "646" | "ansi_x3_4_1968" | "ansi_x3_4_1986" | "cp367" | "csascii"
            | "ibm367" | "iso646_us" | "iso_646_irv_1991" | "iso_ir_6" | "us" | "us_ascii" => {
                Some(PythonCodec::Ascii)
            }
            "latin_1" | "8859" | "cp819" | "csisolatin1" | "ibm819" | "iso8859" | "iso8859_1"
            | "iso_8859_1" | "iso_8859_1_1987" | "iso_ir_100" | "l1" | "latin" | "latin1" => {
                Some(PythonCodec::Latin1)
            }
            "utf_8" | "cp65001" | "u8" | "utf" | "utf8" | "utf8_ucs2" | "utf8_ucs4" => {
                Some(PythonCodec::Utf8)
            }
            "utf_16" | "u16" | "utf16" => Some(PythonCodec::Utf16),
            "utf_16_le" | "unicodelittleunmarked" | "utf_16le" => Some(PythonCodec::Utf16Le),
            "utf_16_be" | "unicodebigunmarked" | "utf_16be" => Some(PythonCodec::Utf16Be),
            _ => None,
        })
}

fn codec_error() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "MCP stdio text encoding failed")
}

pub(super) struct EncodedTransport {
    inner: AsyncRwTransport<RoleClient, ReadHalf<DuplexStream>, WriteHalf<DuplexStream>>,
    cancel: CancellationToken,
    failed: Arc<AtomicBool>,
    task: Option<tokio::task::JoinHandle<io::Result<()>>>,
}
pub(super) fn spawn(
    mut command: tokio::process::Command,
    codec: TextCodec,
) -> Result<(EncodedTransport, Arc<AtomicBool>)> {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| safe_error(ToolErrorKind::ExecutionFailed, "MCP process spawn failed"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| safe_error(ToolErrorKind::ExecutionFailed, "MCP stdout unavailable"))?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| safe_error(ToolErrorKind::ExecutionFailed, "MCP stdin unavailable"))?;
    let (sdk, bridge) = tokio::io::duplex(64 * 1024);
    let (sdk_read, sdk_write) = tokio::io::split(sdk);
    let (bridge_read, bridge_write) = tokio::io::split(bridge);
    let cancel = CancellationToken::new();
    let token = cancel.clone();
    let failed = Arc::new(AtomicBool::new(false));
    let failure_flag = failed.clone();
    let task = tokio::spawn(async move {
        let mut stdout = stdout;
        let mut stdin = stdin;
        let mut bridge_read = bridge_read;
        let mut bridge_write = bridge_write;
        let result = async {
            let result = tokio::select! {
                biased;
                () = token.cancelled() => Ok(()),
                result = async {
                    tokio::try_join!(
                        codec.decode(&mut stdout, &mut bridge_write),
                        codec.encode_stream(&mut bridge_read, &mut stdin)
                    )
                } => result.map(|_| ()),
            };
            // Split duplex halves share their stream. Publish EOF before waiting on the child,
            // and close stdin explicitly so neither peer waits for our process-exit deadline.
            let shutdown = bridge_write.shutdown().await;
            drop((stdout, stdin, bridge_read, bridge_write));
            match tokio::time::timeout(Duration::from_secs(3), child.wait()).await {
                Ok(status) => {
                    status?;
                }
                Err(_) => child.kill().await?,
            }
            result.and(shutdown)
        }
        .await;
        failure_flag.store(result.is_err(), Ordering::Release);
        result
    });
    Ok((
        EncodedTransport {
            inner: AsyncRwTransport::new(sdk_read, sdk_write),
            cancel,
            task: Some(task),
            failed: failed.clone(),
        },
        failed,
    ))
}
impl Transport<RoleClient> for EncodedTransport {
    type Error = io::Error;
    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleClient>,
    ) -> impl Future<Output = io::Result<()>> + Send + 'static {
        self.inner.send(item)
    }
    async fn receive(&mut self) -> Option<RxJsonRpcMessage<RoleClient>> {
        self.inner.receive().await
    }
    async fn close(&mut self) -> io::Result<()> {
        self.cancel.cancel();
        self.inner.close().await?;
        if let Some(task) = self.task.take() {
            if let Ok(result) = task.await {
                result?;
            } else {
                self.failed.store(true, Ordering::Release);
                return Err(io::Error::other("MCP stdio worker failed"));
            }
        }
        Ok(())
    }
}
impl Drop for EncodedTransport {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}
