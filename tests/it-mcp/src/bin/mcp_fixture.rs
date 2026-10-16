//! A real stdio MCP server for transport acceptance.
#[path = "../../support/mod.rs"]
mod support;
use rmcp::ServiceExt;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

#[tokio::main]
async fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "--exit-early") {
        return;
    }
    if args.iter().any(|arg| arg == "--delay-init") {
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
    }
    let fixture = support::Fixture::new("stdio");
    if let Some(wire) = args.iter().find_map(|arg| arg.strip_prefix("--wire=")) {
        // Deliberately independent codec fixtures: no production transcoder is reused here.
        let wire = wire.to_owned();
        let (server, bridge) = tokio::io::duplex(64 * 1024);
        let (read, mut write) = tokio::io::split(bridge);
        let input_wire = wire.clone();
        let input = tokio::spawn(async move {
            let mut stdin = BufReader::new(tokio::io::stdin());
            // `return` leaves this block; the split write half must then publish EOF itself,
            // because the read half keeps the shared duplex stream alive.
            async {
                loop {
                    let text = if input_wire.starts_with("utf16") {
                        let mut units = Vec::new();
                        loop {
                            let next = if input_wire == "utf16le" || input_wire == "utf16" {
                                stdin.read_u16_le().await
                            } else {
                                stdin.read_u16().await
                            };
                            let Ok(unit) = next else {
                                return;
                            };
                            if unit == 0xfeff && units.is_empty() {
                                continue;
                            }
                            units.push(unit);
                            if unit == 10 {
                                break;
                            }
                        }
                        String::from_utf16(&units).unwrap()
                    } else if input_wire == "latin1" {
                        let mut line = Vec::new();
                        if stdin.read_until(b'\n', &mut line).await.unwrap() == 0 {
                            return;
                        }
                        line.into_iter().map(char::from).collect()
                    } else {
                        let mut line = String::new();
                        if stdin.read_line(&mut line).await.unwrap() == 0 {
                            return;
                        }
                        line
                    };
                    if write.write_all(text.as_bytes()).await.is_err() {
                        return;
                    }
                }
            }
            .await;
            let _ = write.shutdown().await;
        });
        let output = tokio::spawn(async move {
            let mut read = BufReader::new(read);
            let mut stdout = tokio::io::stdout();
            if wire == "utf16" {
                stdout.write_all(&[0xff, 0xfe]).await.unwrap();
            }
            loop {
                let mut text = String::new();
                if read.read_line(&mut text).await.unwrap() == 0 {
                    return;
                }
                let bytes: Vec<_> = match wire.as_str() {
                    "latin1" => text
                        .chars()
                        .map(|character| u8::try_from(u32::from(character)).unwrap_or(b'?'))
                        .collect(),
                    "utf16le" | "utf16" => text.encode_utf16().flat_map(u16::to_le_bytes).collect(),
                    "utf16be" => text.encode_utf16().flat_map(u16::to_be_bytes).collect(),
                    "malformed" => text
                        .as_bytes()
                        .windows(2)
                        .position(|pair| pair == "é".as_bytes())
                        .map_or_else(
                            || text.as_bytes().to_vec(),
                            |index| {
                                let mut bytes = text.as_bytes().to_vec();
                                bytes.splice(index..index + 2, [0xff]);
                                bytes
                            },
                        ),
                    _ => panic!("unknown wire mode"),
                };
                if stdout.write_all(&bytes).await.is_err() {
                    return;
                }
                stdout.flush().await.unwrap();
            }
        });
        fixture
            .serve(server)
            .await
            .unwrap()
            .waiting()
            .await
            .unwrap();
        input.abort();
        output.abort();
    } else {
        fixture
            .serve(rmcp::transport::stdio())
            .await
            .unwrap()
            .waiting()
            .await
            .unwrap();
    }
}
