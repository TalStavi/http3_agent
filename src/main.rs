use quinn::{crypto::rustls::QuicClientConfig, ConnectError, Endpoint};
use rustls::RootCertStore;
use rustls::pki_types::CertificateDer;
use serde_json::{json, Value};
use rmp_serde::to_vec;
use rmp_serde::encode::Error as RmpEncodeError;
use std::net::SocketAddr;
use rand::{thread_rng, Rng};
use chrono::Utc;
use clap::Parser;
use tokio::time::{Duration, Instant};
use futures::future::join_all;
use std::sync::Arc;
use bytes::{BytesMut, BufMut};
use thiserror::Error;
use std::net::AddrParseError;
use tokio::sync::mpsc;

#[derive(Error, Debug)]
enum ClientError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Quinn connection error: {0}")]
    QuinnConnection(#[from] quinn::ConnectionError),
    #[error("Quinn write error: {0}")]
    QuinnWrite(#[from] quinn::WriteError),
    #[error("MessagePack encode error: {0}")]
    MessagePackEncode(#[from] RmpEncodeError),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("TLS error: {0}")]
    Tls(#[from] rustls::Error),
    #[error("Connect error: {0}")]
    Connect(#[from] ConnectError),
    #[error("Address parse error: {0}")]
    AddrParse(#[from] AddrParseError),
    #[error("Max retries exceeded")]
    MaxRetriesExceeded,
}

#[derive(Parser, Debug, Clone)]
#[clap(author, version, about, long_about = None)]
struct Args {
    #[clap(short, long, default_value = "127.0.0.1:4433")]
    server_addr: String,
    #[clap(short, long, default_value = "server_cert.der")]
    cert_path: String,
    #[clap(short, long)]
    debug: bool,
    #[clap(long)]
    stress_test: bool,
    #[clap(long, default_value = "200")]
    concurrent_connections: usize,
    #[clap(long, default_value = "5")]
    batch_size: usize,
    #[clap(long, default_value = "20")]
    stress_test_duration: u64,
    #[clap(long, default_value = "20")]
    json_pool_size: usize,
    #[clap(long, default_value = "10")]
    connection_pool_size: usize,
}

struct StressTestMetrics {
    requests_sent: usize,
    jsons_sent: usize,
    total_msgpack_size: usize,
    errors: Vec<String>,
}

struct ConnectionPool {
    connections: Vec<quinn::Connection>,
}

impl ConnectionPool {
    async fn new(endpoint: &Endpoint, server_addr: SocketAddr, size: usize) -> Result<Self, ClientError> {
        let mut connections = Vec::with_capacity(size);
        for _ in 0..size {
            let conn = endpoint.connect(server_addr, "localhost")?.await?;
            connections.push(conn);
        }
        Ok(Self { connections })
    }

    fn get_connection(&self) -> quinn::Connection {
        self.connections[thread_rng().gen_range(0..self.connections.len())].clone()
    }
}

#[tokio::main]
async fn main() -> Result<(), ClientError> {
    let args = Args::parse();
    rustls::crypto::ring::default_provider().install_default().expect("Failed to install rustls crypto provider");
    let cert = std::fs::read(&args.cert_path)?;
    let cert = CertificateDer::from(cert);
    let client_config = create_client_config(cert)?;
    let client_config2 =
        quinn::ClientConfig::new(Arc::new(QuicClientConfig::try_from(client_config).unwrap()));

    let mut endpoint = Endpoint::client("0.0.0.0:0".parse()?)?;
    endpoint.set_default_client_config(client_config2);

    let server_addr: SocketAddr = args.server_addr.parse().expect("Valid address");

    if args.stress_test {
        run_stress_test(&endpoint, server_addr, args).await?;
    } else {
        run_single_request(&endpoint, server_addr, &args).await?;
    }

    Ok(())
}

async fn run_stress_test(endpoint: &Endpoint, server_addr: SocketAddr, args: Args) -> Result<(), ClientError> {
    println!("Starting stress test...");
    
    let start_time = Instant::now();
    let duration = Duration::from_secs(args.stress_test_duration);

    let json_pool = Arc::new(generate_json_pool(args.json_pool_size));

    let pool = Arc::new(ConnectionPool::new(endpoint, server_addr, args.connection_pool_size).await?);

    let (tx, mut rx) = mpsc::channel(args.concurrent_connections);

    let collector_handle = tokio::spawn(async move {
        let mut metrics = StressTestMetrics {
            requests_sent: 0,
            jsons_sent: 0,
            total_msgpack_size: 0,
            errors: Vec::new(),
        };

        while let Some(result) = rx.recv().await {
            match result {
                Ok((requests, jsons, size)) => {
                    metrics.requests_sent += requests;
                    metrics.jsons_sent += jsons;
                    metrics.total_msgpack_size += size;
                }
                Err(e) => {
                    metrics.errors.push(format!("Batch error: {:?}", e));
                }
            }
        }

        metrics
    });

    let mut handles = Vec::new();
    for _ in 0..args.concurrent_connections {
        let tx = tx.clone();
        let args_clone = args.clone();
        let json_pool_clone = Arc::clone(&json_pool);
        let pool_clone = Arc::clone(&pool);
        handles.push(tokio::spawn(async move {
            while start_time.elapsed() < duration {
                let result = send_batch_with_retry(&pool_clone, &args_clone, &json_pool_clone).await;
                if tx.send(result).await.is_err() {
                    break;
                }
            }
        }));
    }

    join_all(handles).await;
    drop(tx);

    let metrics = collector_handle.await.unwrap();
    let elapsed = start_time.elapsed();
    
    println!("Stress test completed in {:.2} seconds", elapsed.as_secs_f64());
    println!("Requests sent: {}", metrics.requests_sent);
    println!("JSONs sent: {}", metrics.jsons_sent);
    println!("Average msgpack size per batch: {:.2} bytes", metrics.total_msgpack_size as f64 / metrics.requests_sent as f64);
    println!("Requests per second: {:.2}", metrics.requests_sent as f64 / elapsed.as_secs_f64());
    println!("JSONs per second: {:.2}", metrics.jsons_sent as f64 / elapsed.as_secs_f64());
    println!("Errors encountered: {}", metrics.errors.len());
    for error in &metrics.errors {
        println!("Error: {}", error);
    }

    Ok(())
}

async fn send_batch_with_retry(
    pool: &Arc<ConnectionPool>,
    args: &Args,
    json_pool: &[Value],
) -> Result<(usize, usize, usize), ClientError> {
    const MAX_RETRIES: usize = 3;
    let mut retries = 0;

    while retries < MAX_RETRIES {
        let connection = pool.get_connection();
        match send_batch(&connection, args, json_pool).await {
            Ok(result) => return Ok(result),
            Err(ClientError::QuinnWrite(quinn::WriteError::Stopped(_))) => {
                retries += 1;
                if retries < MAX_RETRIES {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            },
            Err(e) => return Err(e),
        }
    }

    Err(ClientError::MaxRetriesExceeded)
}

async fn send_batch(
    connection: &quinn::Connection,
    args: &Args,
    json_pool: &[Value],
) -> Result<(usize, usize, usize), ClientError> {
    let mut send = connection.open_uni().await?;

    let mut batch = Vec::new();
    for _ in 0..args.batch_size {
        let process_data = select_random_json(json_pool);
        batch.push(process_data);
    }

    let batch_msgpack = to_vec(&batch)?;
    send_length_prefixed_message(&mut send, &batch_msgpack).await?;

    if args.debug {
        println!("Sent batch of {} messages", args.batch_size);
    }

    Ok((1, args.batch_size, batch_msgpack.len()))
}

async fn run_single_request(endpoint: &Endpoint, server_addr: SocketAddr, args: &Args) -> Result<(), ClientError> {
    let connection = endpoint.connect(server_addr, "localhost")?.await?;
    println!("Connected to {}", server_addr);

    let mut send = connection.open_uni().await?;

    let process_data = generate_fake_process_data();
    let data_to_send = vec![process_data]; // Wrap in an array
    let msgpack_data = to_vec(&data_to_send)?;
    send_length_prefixed_message(&mut send, &msgpack_data).await?;

    println!("Sent data: {}", serde_json::to_string_pretty(&data_to_send)?);

    if args.debug {
        println!("Data sent successfully. No acknowledgment expected.");
    }

    Ok(())
}

fn create_client_config(server_cert: CertificateDer) -> Result<rustls::ClientConfig, ClientError> {
    let mut roots = RootCertStore::empty();
    roots.add(server_cert)?;
    
    let client_crypto = rustls::ClientConfig::builder()
    .with_root_certificates(roots)
    .with_no_client_auth();

    Ok(client_crypto)
}

fn generate_json_pool(pool_size: usize) -> Vec<Value> {
    (0..pool_size).map(|_| generate_fake_process_data()).collect()
}

fn select_random_json(json_pool: &[Value]) -> Value {
    let mut rng = thread_rng();
    json_pool[rng.gen_range(0..json_pool.len())].clone()
}

async fn send_length_prefixed_message(send: &mut quinn::SendStream, data: &[u8]) -> Result<(), ClientError> {
    let length = data.len() as u32;
    let length_bytes = length.to_be_bytes();
    
    let mut message = BytesMut::with_capacity(4 + data.len());
    message.put_slice(&length_bytes);
    message.put_slice(data);

    send.write_all(&message).await?;
    let _ = send.finish();
    Ok(())
}

fn generate_fake_process_data() -> Value {
    let mut rng = thread_rng();
    
    let process_names = ["chrome.exe", "firefox.exe", "notepad.exe", "explorer.exe"];
    let program_folders = ["Google", "Mozilla", "Windows NT", "Internet Explorer"];
    let program_names = ["chrome", "firefox", "notepad", "iexplore"];
    let user_names = ["ADMIN", "JOHN", "JANE", "GUEST"];
    let integrity_levels = ["Low", "Medium", "High", "System"];

    json!({
        "event_type": "process_creation",
        "timestamp": Utc::now().to_rfc3339(),
        "process": {
            "pid": rng.gen_range(1000..10000),
            "name": process_names[rng.gen_range(0..process_names.len())],
            "path": format!("C:\\Program Files\\{}\\{}.exe", 
                            program_folders[rng.gen_range(0..program_folders.len())],
                            program_names[rng.gen_range(0..program_names.len())]),
            "command_line": format!("{}.exe --flag1 --flag2=\"some value\"", 
                                    program_names[rng.gen_range(0..program_names.len())]),
            "user": format!("USER-{}", user_names[rng.gen_range(0..user_names.len())]),
            "integrity_level": integrity_levels[rng.gen_range(0..integrity_levels.len())],
        },
        "parent_process": {
            "pid": rng.gen_range(1..1000),
            "name": "explorer.exe",
            "path": "C:\\Windows\\explorer.exe",
        },
        "network": {
            "local_ip": format!("192.168.1.{}", rng.gen_range(1..255)),
            "local_port": rng.gen_range(1024..65535),
            "remote_ip": format!("{}.{}.{}.{}", rng.gen_range(1..255), rng.gen_range(1..255), rng.gen_range(1..255), rng.gen_range(1..255)),
            "remote_port": rng.gen_range(1..65535),
        },
        "file_system": {
            "files_accessed": [
                format!("C:\\Users\\{}\\Documents\\file{}.txt", user_names[rng.gen_range(0..user_names.len())], rng.gen_range(1..10)),
                format!("C:\\Program Files\\file{}.dll", rng.gen_range(1..10)),
            ],
        },
        "registry": {
            "keys_accessed": [
                format!("HKEY_LOCAL_MACHINE\\SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Run\\Program{}", rng.gen_range(1..10)),
                format!("HKEY_CURRENT_USER\\Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\User Shell Folders\\Desktop"),
            ],
        },
    })
}